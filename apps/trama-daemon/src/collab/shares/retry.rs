//! Durable delivery of share operations that cannot be rolled back once
//! the local MLS group moved: membership commits (`membership_changed`),
//! Welcomes and the snapshot republish that follows. Operations are
//! queued in TramaDB, flushed in order per share right away, before any
//! other event of that share, and every [`RETRY_EVERY`] in the background.
//! Nothing is rolled back: a failed attempt only bumps the attempt count.

use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use trama_collab_protocol::vnext::api::WelcomeBlob;
use trama_collab_protocol::vnext::events::ShareEventEnvelope;

use super::super::CollabService;
use crate::error::{DaemonError, Result};

pub(crate) const OP_EVENT: &str = "event";
pub(crate) const OP_WELCOME: &str = "welcome";
pub(crate) const OP_SNAPSHOT: &str = "snapshot";
pub(crate) const RETRY_EVERY: Duration = Duration::from_secs(20);

impl CollabService {
    pub(crate) async fn enqueue_welcome(&self, share_id: &str, blob: &WelcomeBlob) -> Result<()> {
        let body = serde_json::to_string(blob).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        self.db()
            .enqueue_share_op(share_id, OP_WELCOME, &body)
            .await?;
        Ok(())
    }

    pub(crate) async fn enqueue_snapshot(&self, share_id: &str) -> Result<()> {
        self.db()
            .enqueue_share_op(share_id, OP_SNAPSHOT, "{}")
            .await?;
        Ok(())
    }

    /// Sends pending operations (of `share_id`, or all), oldest first,
    /// stopping a share at its first failure. True when none of the
    /// requested share's operations remain.
    pub(crate) async fn flush_share_ops(&self, share_id: Option<&str>) -> bool {
        let Ok(ops) = self.db().share_pending_ops().await else {
            return false;
        };
        let Ok((_, client)) = self.share_client() else {
            return !ops.iter().any(|op| Some(op.share_id.as_str()) == share_id);
        };
        let mut blocked: Vec<String> = Vec::new();
        for op in ops {
            if share_id.is_some_and(|s| s != op.share_id) || blocked.contains(&op.share_id) {
                continue;
            }
            let outcome: Result<()> = match op.kind.as_str() {
                OP_EVENT => match serde_json::from_str::<ShareEventEnvelope>(&op.body) {
                    Ok(event) => client.publish(&op.share_id, vec![event]).await.map(|_| ()),
                    Err(e) => Err(DaemonError::Invalid(e.to_string())),
                },
                OP_WELCOME => match serde_json::from_str::<WelcomeBlob>(&op.body) {
                    Ok(blob) if STANDARD.decode(&blob.welcome).is_ok() => {
                        client.post_welcome(&op.share_id, &blob).await
                    }
                    _ => Err(DaemonError::Invalid("stored welcome is malformed".into())),
                },
                OP_SNAPSHOT => self
                    .publish_history_snapshot(&client, &op.share_id)
                    .await
                    .map(|_| ()),
                other => Err(DaemonError::Invalid(format!("unknown share op {other}"))),
            };
            match outcome {
                Ok(()) => {
                    let _ = self.db().finish_share_op(op.id).await;
                }
                Err(err) => {
                    crate::log::warn(format!(
                        "share {} {} not delivered (attempt {}): {err}",
                        op.share_id,
                        op.kind,
                        op.attempts + 1
                    ));
                    let _ = self.db().fail_share_op(op.id, &err.to_string()).await;
                    blocked.push(op.share_id.clone());
                }
            }
        }
        match share_id {
            Some(id) => !blocked.iter().any(|b| b == id),
            None => blocked.is_empty(),
        }
    }

    /// Background retry loop; runs for the daemon's lifetime.
    pub(crate) fn start_share_retry(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RETRY_EVERY).await;
                if service.connection(None).is_some() {
                    service.flush_share_ops(None).await;
                }
            }
        });
    }
}
