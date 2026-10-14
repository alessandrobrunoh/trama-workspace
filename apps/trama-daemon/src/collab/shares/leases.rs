//! Writer leases on shared threads (docs/plan/03 §5).

use trama_collab_protocol::vnext::api::{LeaseInfo, LeaseRequest};
use trama_domain::ids::ThreadId;
use trama_domain::share::ShareId;
use trama_protocol::ShareLeaseSnapshot;

use super::super::CollabService;
use crate::error::{DaemonError, Result};

impl CollabService {
    async fn thread_gid_of(&self, share_id: &str, thread: &ThreadId) -> Result<String> {
        let rows = self.db().share_threads(&ShareId::from(share_id)).await?;
        rows.into_iter()
            .find(|r| r.local_thread_id.as_deref() == Some(&thread.to_string()))
            .map(|r| r.thread_gid.0)
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread} in share {share_id}")))
    }

    async fn lease_snapshot(&self, share_id: &str, lease: LeaseInfo) -> Result<ShareLeaseSnapshot> {
        let thread_id = self
            .local_thread(&ShareId::from(share_id), Some(&lease.thread_gid))
            .await?;
        Ok(ShareLeaseSnapshot {
            share_id: share_id.to_owned(),
            thread_id,
            thread_gid: lease.thread_gid,
            holder: lease.holder,
            device_id: lease.device_id,
            expires_at: lease.expires_at,
        })
    }

    pub(crate) async fn acquire_share_lease(
        &self,
        share_id: &str,
        thread: &ThreadId,
        ttl_secs: u64,
    ) -> Result<ShareLeaseSnapshot> {
        let device_id = self.e2ee()?.device_id().to_owned();
        let (_, client) = self.share_client()?;
        let thread_gid = self.thread_gid_of(share_id, thread).await?;
        let lease = client
            .acquire_lease(
                share_id,
                &LeaseRequest {
                    thread_gid,
                    device_id,
                    ttl_secs,
                },
            )
            .await?;
        self.lease_snapshot(share_id, lease).await
    }

    pub(crate) async fn release_share_lease(
        &self,
        share_id: &str,
        thread: &ThreadId,
    ) -> Result<()> {
        let (_, client) = self.share_client()?;
        let thread_gid = self.thread_gid_of(share_id, thread).await?;
        client.release_lease(share_id, &thread_gid).await
    }

    pub(crate) async fn list_share_leases(
        &self,
        share_id: &str,
    ) -> Result<Vec<ShareLeaseSnapshot>> {
        let (_, client) = self.share_client()?;
        let mut out = Vec::new();
        for lease in client.leases(share_id).await? {
            out.push(self.lease_snapshot(share_id, lease).await?);
        }
        Ok(out)
    }
}
