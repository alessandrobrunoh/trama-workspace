//! Share health for the desktop: persisted warnings (`content_skipped`, …)
//! and the joiner's checkout-restore state, with a manual retry.
//! Restore state lives in `collab_settings` (`share_restore:<share>` holds
//! the JSON below); warnings in `share_warnings`.

use serde::{Deserialize, Serialize};
use trama_domain::share::ShareId;
use trama_protocol::{
    RestoredCheckout, ShareRestoreState, ShareRole, ShareStatusReport, ShareWarning,
};

use super::super::CollabService;
use super::super::transport::ShareClient;
use super::flow::role_key;
use super::snapshot::VerifiedSnapshot;
use crate::error::{DaemonError, Result};

#[derive(Serialize, Deserialize)]
struct RestoreRecord {
    state: ShareRestoreState,
    /// Snapshot version the checkouts were last restored or updated from.
    #[serde(default)]
    version: u64,
    error: Option<String>,
    checkouts: Vec<RestoredCheckout>,
}

fn restore_key(share_id: &str) -> String {
    format!("share_restore:{share_id}")
}

impl CollabService {
    /// Restores and records the outcome; never fails the caller (history
    /// stays usable without code).
    pub(crate) async fn run_share_restore(
        &self,
        client: &ShareClient,
        share_id: &str,
        snapshot: &VerifiedSnapshot,
    ) -> Result<()> {
        let outcome = self.restore_share_content(client, share_id, snapshot).await;
        self.record_restore(share_id, snapshot.version, outcome)
            .await
    }

    async fn record_restore(
        &self,
        share_id: &str,
        version: u64,
        outcome: Result<Option<Vec<RestoredCheckout>>>,
    ) -> Result<()> {
        let record = match outcome {
            Ok(None) => RestoreRecord {
                state: ShareRestoreState::NoContent,
                version,
                error: None,
                checkouts: Vec::new(),
            },
            Ok(Some(checkouts)) => RestoreRecord {
                state: ShareRestoreState::Restored,
                version,
                error: None,
                checkouts,
            },
            Err(e) => {
                crate::log::warn(format!("share {share_id}: content not restored: {e}"));
                RestoreRecord {
                    state: ShareRestoreState::Failed,
                    version,
                    error: Some(e.to_string()),
                    checkouts: Vec::new(),
                }
            }
        };
        let body =
            serde_json::to_string(&record).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        self.db()
            .set_collab_setting(&restore_key(share_id), &body)
            .await?;
        Ok(())
    }

    pub(crate) async fn share_status(&self, share_id: &str) -> Result<ShareStatusReport> {
        if self.db().share(&ShareId::from(share_id)).await?.is_none() {
            return Err(DaemonError::NotFound(format!("share {share_id}")));
        }
        let stored = self
            .db()
            .collab_setting_or_insert(&restore_key(share_id), "")
            .await?;
        let record = serde_json::from_str::<RestoreRecord>(&stored).unwrap_or(RestoreRecord {
            state: ShareRestoreState::NotRequested,
            version: 0,
            error: None,
            checkouts: Vec::new(),
        });
        let warnings = self
            .db()
            .share_warnings(share_id)
            .await?
            .into_iter()
            .map(|w| ShareWarning {
                code: w.code,
                message: w.message,
                at: w.created_at,
            })
            .collect();
        Ok(ShareStatusReport {
            share_id: share_id.to_owned(),
            restore: record.state,
            restore_error: record.error,
            checkouts: record.checkouts,
            update_available: match record.state {
                ShareRestoreState::Restored => {
                    self.update_available(share_id, record.version).await
                }
                _ => None,
            },
            warnings,
        })
    }

    /// Joiner with `read_code`: verifies the latest snapshot again and
    /// restores whatever is missing.
    pub(crate) async fn retry_share_restore(&self, share_id: &str) -> Result<ShareStatusReport> {
        let client = self.code_client(share_id).await?;
        let snapshot = self.read_verified_snapshot(&client, share_id).await?;
        self.run_share_restore(&client, share_id, &snapshot).await?;
        self.share_status(share_id).await
    }

    /// Best effort (never fails `GetShareStatus`): the latest snapshot
    /// version when it is newer than the restore and moves a checkout.
    async fn update_available(&self, share_id: &str, restored: u64) -> Option<u64> {
        let check = async {
            let (_, client) = self.share_client().ok()?;
            let info = client.info(share_id).await.ok()?;
            if info.latest_manifest_version <= restored {
                return None;
            }
            let snapshot = self.read_verified_snapshot(&client, share_id).await.ok()?;
            let manifest = self
                .content_manifest(&client, share_id, &snapshot)
                .await
                .ok()??;
            self.checkouts_behind(share_id, &manifest)
                .await
                .ok()?
                .then_some(snapshot.version)
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), check)
            .await
            .ok()
            .flatten()
    }

    /// Joiner: applies the latest snapshot to the attached checkouts.
    /// Refused (nothing changed) when one has local changes.
    pub(crate) async fn apply_share_update(&self, share_id: &str) -> Result<ShareStatusReport> {
        let client = self.code_client(share_id).await?;
        let snapshot = self.read_verified_snapshot(&client, share_id).await?;
        let outcome = self
            .update_share_content(&client, share_id, &snapshot)
            .await;
        if let Err(e @ DaemonError::Invalid(_)) = outcome {
            // Local changes: keep the previous record, report the refusal.
            return Err(e);
        }
        self.record_restore(share_id, snapshot.version, outcome)
            .await?;
        self.share_status(share_id).await
    }

    async fn code_client(&self, share_id: &str) -> Result<ShareClient> {
        let role = self
            .db()
            .collab_setting_or_insert(&role_key(share_id), "reader")
            .await?;
        if role == "owner" {
            return Err(DaemonError::Invalid(
                "the share owner has the original checkouts".into(),
            ));
        }
        let (_, client) = self.share_client()?;
        let info = client.info(share_id).await?;
        if info.role == ShareRole::Owner || !info.read_code {
            return Err(DaemonError::Invalid(
                "this device has no access to the share's code".into(),
            ));
        }
        Ok(client)
    }
}
