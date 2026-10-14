//! Share/Join flow (protocols 14–16); the work lives in `collab::shares`.
//! Failures get a correlation id: `ShareFailed` then `Error` (message
//! suffixed with `[ref <id>]`), and the daemon log carries the same id.

use super::*;
use trama_domain::ids::MessageId;

fn share_id_of(msg: &ClientMessage) -> Option<String> {
    serde_json::to_value(msg)
        .ok()?
        .get("payload")?
        .get("share_id")?
        .as_str()
        .map(str::to_owned)
}

pub(super) async fn handle(msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let operation = serde_json::to_value(&msg)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_default();
    let share_id = share_id_of(&msg);
    let reply = match run(msg, ctx.collab).await {
        Ok(Some(reply)) => reply,
        Ok(None) => return Ok(ConnectionControl::Continue),
        Err(err) => {
            let correlation_id = format!("shr_{}", MessageId::generate());
            crate::log::warn(format!(
                "share command {operation} failed [ref {correlation_id}]: {err}"
            ));
            let revoked = match &share_id {
                Some(id) => ctx.collab.check_share_access(id).await,
                None => false,
            };
            let code = if revoked {
                "access_revoked"
            } else {
                err.code()
            }
            .to_owned();
            let message = err.to_string();
            let _ = ctx.out_tx.send(DaemonMessage::ShareFailed {
                operation,
                share_id,
                correlation_id: correlation_id.clone(),
                code: code.clone(),
                message: message.clone(),
            });
            DaemonMessage::Error {
                code,
                message: format!("{message} [ref {correlation_id}]"),
            }
        }
    };
    let _ = ctx.out_tx.send(reply);
    Ok(ConnectionControl::Continue)
}

async fn run(msg: ClientMessage, collab: &CollabService) -> Result<Option<DaemonMessage>> {
    let reply = match msg {
        ClientMessage::SharePreflight { thread_id, scope } => {
            DaemonMessage::SharePreflightReport(collab.share_preflight(&thread_id, scope).await?)
        }
        ClientMessage::CreateShare {
            thread_id,
            scope,
            members,
        } => match collab.create_share(&thread_id, scope, members).await? {
            Ok(share) => DaemonMessage::ShareCreated(share),
            Err(report) => DaemonMessage::SharePreflightReport(report),
        },
        ClientMessage::ListShares => DaemonMessage::Shares(collab.list_shares().await?),
        ClientMessage::RequestShareJoin { share_id } => {
            let device_id = collab.request_share_join(&share_id).await?;
            DaemonMessage::ShareJoinRequested {
                share_id,
                device_id,
            }
        }
        ClientMessage::ListShareJoinRequests { share_id } => {
            let requests = collab.list_share_join_requests(&share_id).await?;
            DaemonMessage::ShareJoinRequests { share_id, requests }
        }
        ClientMessage::ApproveShareJoin {
            share_id,
            device_id,
        } => {
            let key_epoch = collab.approve_share_join(&share_id, &device_id).await?;
            DaemonMessage::ShareJoinApproved {
                share_id,
                device_id,
                key_epoch,
            }
        }
        ClientMessage::CompleteShareJoin {
            share_id,
            workspace_id,
        } => {
            DaemonMessage::ShareJoined(collab.complete_share_join(&share_id, &workspace_id).await?)
        }
        ClientMessage::PublishShareSnapshot { share_id } => {
            let version = collab.publish_share_snapshot(&share_id).await?;
            DaemonMessage::ShareSnapshotPublished { share_id, version }
        }
        ClientMessage::AcquireShareLease {
            share_id,
            thread_id,
            ttl_secs,
        } => DaemonMessage::ShareLease(
            collab
                .acquire_share_lease(&share_id, &thread_id, ttl_secs)
                .await?,
        ),
        ClientMessage::ReleaseShareLease {
            share_id,
            thread_id,
        } => {
            collab.release_share_lease(&share_id, &thread_id).await?;
            DaemonMessage::ShareLeaseReleased {
                share_id,
                thread_id,
            }
        }
        ClientMessage::ListShareLeases { share_id } => {
            let leases = collab.list_share_leases(&share_id).await?;
            DaemonMessage::ShareLeases { share_id, leases }
        }
        ClientMessage::RemoveShareMember { share_id, username } => {
            let (devices, key_epoch) = collab.remove_share_member(&share_id, &username).await?;
            DaemonMessage::ShareMemberRemoved {
                share_id,
                username,
                devices,
                key_epoch,
            }
        }
        ClientMessage::ListShareMembers { share_id } => {
            let members = collab.list_share_members(&share_id).await?;
            DaemonMessage::ShareMembers { share_id, members }
        }
        ClientMessage::GetShareHistoryStatus { thread_id } => {
            let entries = collab.share_history_status(&thread_id).await?;
            DaemonMessage::ShareHistoryStatus { thread_id, entries }
        }
        ClientMessage::GetShareStatus { share_id } => {
            DaemonMessage::ShareStatusReport(collab.share_status(&share_id).await?)
        }
        ClientMessage::RetryShareRestore { share_id } => {
            DaemonMessage::ShareStatusReport(collab.retry_share_restore(&share_id).await?)
        }
        ClientMessage::ApplyShareUpdate { share_id } => {
            DaemonMessage::ShareStatusReport(collab.apply_share_update(&share_id).await?)
        }
        ClientMessage::GetThreadShare { thread_id } => {
            let share = collab.thread_share(&thread_id).await?;
            DaemonMessage::ThreadShare { thread_id, share }
        }
        _ => return Ok(None),
    };
    Ok(Some(reply))
}
