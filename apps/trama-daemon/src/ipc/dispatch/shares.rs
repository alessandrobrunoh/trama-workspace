//! Share/Join flow (protocol 14); the work lives in `collab::shares`.

use super::*;

pub(super) async fn handle(msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let Ctx { collab, out_tx, .. } = ctx;
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
        _ => return Ok(ConnectionControl::Continue),
    };
    let _ = out_tx.send(reply);
    Ok(ConnectionControl::Continue)
}
