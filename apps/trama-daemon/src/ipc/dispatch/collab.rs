use super::*;

pub(super) async fn handle(msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let Ctx { collab, out_tx, .. } = ctx;
    match msg {
        ClientMessage::ConnectCollab {
            server_url,
            username,
            password,
            runner_token,
        } => {
            let status = collab
                .connect(server_url, username, password, runner_token)
                .await?;
            let _ = out_tx.send(DaemonMessage::CollabStatus(status));
        }
        ClientMessage::DisconnectCollab { forget } => {
            let status = collab.disconnect(forget).await?;
            let _ = out_tx.send(DaemonMessage::CollabStatus(status));
        }
        ClientMessage::GetCollabStatus => {
            let _ = out_tx.send(DaemonMessage::CollabStatus(collab.status().await?));
        }
        ClientMessage::ShareThread {
            thread_id,
            access,
            invite,
        } => {
            let shared = collab.share_thread(&thread_id, access, invite).await?;
            let _ = out_tx.send(DaemonMessage::SharedThread(shared));
        }
        ClientMessage::GetSharedThread { thread_id } => {
            let shared = collab.shared_thread(&thread_id).await?;
            let _ = out_tx.send(DaemonMessage::SharedThread(shared));
        }
        ClientMessage::RemoveSharedThreadMember {
            thread_id,
            username,
        } => {
            let shared = collab.remove_member(&thread_id, &username).await?;
            let _ = out_tx.send(DaemonMessage::SharedThread(shared));
        }
        ClientMessage::JoinSharedThread {
            workspace_id,
            collab_thread_id,
            link_token,
        } => {
            let (thread, shared) = collab
                .join(workspace_id, collab_thread_id, link_token)
                .await?;
            let _ = out_tx.send(DaemonMessage::SharedThreadJoined { thread, shared });
        }
        ClientMessage::ListSharedThreads => {
            let _ = out_tx.send(DaemonMessage::SharedThreads(collab.list_shared().await?));
        }
        ClientMessage::UnlinkSharedThread { thread_id } => {
            collab.unlink(&thread_id).await?;
            let _ = out_tx.send(DaemonMessage::SharedThreadUnlinked { thread_id });
        }
        ClientMessage::SetCollabDraft { thread_id, text } => {
            collab.set_draft(&thread_id, text)?;
        }
        ClientMessage::SetCollabPresence { thread_id, state } => {
            collab.set_presence(&thread_id, state)?;
        }
        _ => unreachable!("not a collab message"),
    }
    Ok(ConnectionControl::Continue)
}
