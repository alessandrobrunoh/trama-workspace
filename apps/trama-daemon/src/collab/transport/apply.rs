//! Applying one incoming share event: replay admission (seeded from the
//! persisted heads), persistence of the new head and stream cursor, then
//! IPC fan-out to the local thread the event maps to.

use trama_collab_protocol::vnext::events::ShareEventEnvelope;
use trama_collab_protocol::vnext::replay::Admission;
use trama_db::ShareDeviceHead;
use trama_domain::ids::ThreadId;
use trama_domain::share::{ShareId, ThreadGid};
use trama_protocol::DaemonMessage;

use super::super::CollabService;
use crate::error::Result;

impl CollabService {
    pub(crate) async fn seed_share(&self, share_id: &str) -> Result<()> {
        if self.share_events().is_tracked(share_id) {
            return Ok(());
        }
        let heads = self
            .db()
            .share_device_heads(&ShareId::from(share_id))
            .await?;
        self.share_events().track(share_id);
        for head in heads {
            self.share_events().restore_head(
                share_id,
                &head.device_id,
                head.author_seq,
                &head.event_id,
            );
        }
        Ok(())
    }

    /// The local thread for `thread_gid`, else the share root's.
    pub(crate) async fn local_thread(
        &self,
        share: &ShareId,
        gid: Option<&str>,
    ) -> Result<Option<ThreadId>> {
        let rows = self.db().share_threads(share).await?;
        let root = self.db().share(share).await?.and_then(|s| s.root_gid);
        let wanted: Option<ThreadGid> = gid.map(ThreadGid::from).or(root);
        Ok(wanted.and_then(|gid| {
            rows.into_iter()
                .find(|r| r.thread_gid == gid)
                .and_then(|r| r.local_thread_id)
                .map(ThreadId::from)
        }))
    }

    /// Admits, persists and forwards `event`. Duplicates are dropped;
    /// gaps and rejects are returned and nothing is stored.
    pub(crate) async fn apply_share_event(&self, event: ShareEventEnvelope) -> Result<Admission> {
        let share_id = event.share_id.clone();
        self.seed_share(&share_id).await?;
        let admission = self.share_events().admit(&event);
        if admission != Admission::Accept {
            return Ok(admission);
        }
        let share = ShareId::from(share_id.as_str());
        self.db()
            .record_share_event(
                &share,
                ShareDeviceHead {
                    device_id: event.author_device.clone(),
                    author_seq: event.author_seq,
                    event_id: event.event_id.clone(),
                },
                event.server_seq,
            )
            .await?;
        if let Err(err) = self.apply_share_payload(&event).await {
            crate::log::warn(format!("share event {} not applied: {err}", event.event_id));
        }
        if let Some(thread) = self
            .local_thread(&share, event.thread_gid.as_deref())
            .await?
        {
            self.sessions().notify(
                &thread,
                DaemonMessage::ShareEvent {
                    thread_id: Some(thread.clone()),
                    event,
                },
            );
        }
        Ok(admission)
    }
}
