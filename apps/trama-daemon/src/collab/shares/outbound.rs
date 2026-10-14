//! Authoring vNext events: next `author_seq` from the persisted own head,
//! payload sealed by the share's MLS group at its current epoch.
//! `signature` carries the marker `mls`: authenticity comes from the MLS
//! framing (sender leaf signature) and the server binds devices to users;
//! a separate per-event signature is deferred (docs/plan/README.md).

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use trama_collab_protocol::vnext::cipher::{PayloadCipher, PayloadKind};
use trama_collab_protocol::vnext::events::{ShareEventEnvelope, ShareEventKind};
use trama_db::ShareDeviceHead;
use trama_domain::ids::MessageId;
use trama_domain::share::ShareId;

use super::super::CollabService;
use super::super::e2ee::mls_err;
use super::super::transport::ShareClient;
use crate::error::{DaemonError, Result};

pub(crate) const MLS_SIGNATURE_MARKER: &str = "mls";

pub(crate) enum Payload {
    /// Sealed with the group's current epoch.
    Plain(Vec<u8>),
    /// Already an MLS message (a commit) for `epoch`.
    Raw { bytes: Vec<u8>, epoch: u64 },
}

impl CollabService {
    pub(crate) fn share_client(&self) -> Result<(super::super::Connection, ShareClient)> {
        let connection = self.require_connection()?;
        let client = ShareClient::from_connection(&connection)?;
        Ok((connection, client))
    }

    /// Seals, publishes and records one event authored by this device.
    pub(crate) async fn emit_share_event(
        &self,
        client: &ShareClient,
        share_id: &str,
        kind: ShareEventKind,
        thread_gid: Option<String>,
        payload: Payload,
    ) -> Result<ShareEventEnvelope> {
        let durable = matches!(payload, Payload::Raw { .. });
        if !durable && !self.flush_share_ops(Some(share_id)).await {
            return Err(DaemonError::Collab(
                "earlier membership changes of this share are still being delivered; try again shortly"
                    .into(),
            ));
        }
        let e2ee = self.e2ee()?;
        let device = e2ee.device_id().to_owned();
        let share = ShareId::from(share_id);
        let heads = self.db().share_device_heads(&share).await?;
        let own = heads.into_iter().find(|h| h.device_id == device);
        let (bytes, key_epoch) = match payload {
            Payload::Raw { bytes, epoch } => (bytes, epoch),
            Payload::Plain(plain) => {
                let id = share_id.to_owned();
                tokio::task::spawn_blocking(move || {
                    let epoch = e2ee
                        .cipher
                        .with_group(&id, |g, _| Ok(g.epoch()))
                        .map_err(mls_err)?;
                    let sealed = e2ee
                        .cipher
                        .seal(&id, epoch, PayloadKind::Event, &plain)
                        .map_err(|e| DaemonError::Collab(e.to_string()))?;
                    Ok::<_, DaemonError>((sealed, epoch))
                })
                .await
                .map_err(|e| DaemonError::Collab(format!("seal task: {e}")))??
            }
        };
        let event = ShareEventEnvelope {
            event_id: format!("ev_{}", MessageId::generate()),
            share_id: share_id.to_owned(),
            kind,
            thread_gid,
            author_device: device.clone(),
            author_seq: own.as_ref().map_or(1, |h| h.author_seq + 1),
            prev_author_event: own.map(|h| h.event_id),
            parents: Vec::new(),
            server_seq: None,
            key_epoch,
            cipher_payload: STANDARD.encode(bytes),
            signature: MLS_SIGNATURE_MARKER.into(),
            created_at: Utc::now().timestamp(),
            received_at: None,
        };
        if durable {
            // Membership commits are already merged locally: they must reach
            // the server eventually, so they go through the retry queue.
            let body =
                serde_json::to_string(&event).map_err(|e| DaemonError::Invalid(e.to_string()))?;
            self.db()
                .enqueue_share_op(share_id, super::retry::OP_EVENT, &body)
                .await?;
        } else {
            client.publish(share_id, vec![event.clone()]).await?;
        }
        // Own head only: the stream cursor must not jump past events of
        // others that were not delivered yet.
        self.db()
            .record_share_event(
                &share,
                ShareDeviceHead {
                    device_id: device,
                    author_seq: event.author_seq,
                    event_id: event.event_id.clone(),
                },
                None,
            )
            .await?;
        self.seed_share(share_id).await?;
        let _ = self.share_events().admit(&event);
        if durable {
            self.flush_share_ops(Some(share_id)).await;
        }
        Ok(event)
    }
}
