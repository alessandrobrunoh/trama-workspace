use super::*;

pub(super) async fn handle(msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let Ctx {
        sessions,
        collab,
        out_tx,
        subscriptions,
        ..
    } = ctx;
    match msg {
        ClientMessage::SendComments { thread_id } => {
            let sent = match collab.link(&thread_id).await {
                Ok(Some(_)) => Err(DaemonError::Invalid(
                    "in a shared thread, pending comments go out with your next message".into(),
                )),
                Ok(None) => sessions.send_comments(thread_id.clone()).await.map(|_| ()),
                Err(error) => Err(error),
            };
            if let Err(error) = sent {
                let message = error.to_string();
                let _ = out_tx.send(DaemonMessage::Error {
                    code: error.code().into(),
                    message: message.clone(),
                });
                let _ = out_tx.send(DaemonMessage::AgentEventEnvelope {
                    thread_id,
                    event: trama_domain::agent::AgentEvent::Failed { message },
                });
            }
        }
        // Comment changes are broadcast to the thread's subscribers; a
        // requester that is not subscribed gets the reply directly.
        ClientMessage::CreateComment {
            thread_id,
            target,
            body,
            parent_comment_id,
        } => {
            let snapshot = sessions
                .create_comment(thread_id.clone(), target, body, parent_comment_id)
                .await?;
            collab.publish_comment(snapshot.clone());
            if !subscriptions.contains_key(&thread_id) {
                let _ = out_tx.send(DaemonMessage::CommentCreated(snapshot));
            }
        }
        ClientMessage::ListComments { thread_id } => {
            let comments = sessions.list_comments(&thread_id).await?;
            let _ = out_tx.send(DaemonMessage::Comments {
                thread_id,
                comments,
            });
        }
        ClientMessage::ResolveComment { comment_id } => {
            let snapshot = sessions.resolve_comment(&comment_id).await?;
            collab.publish_resolve(snapshot.clone());
            if !subscriptions.contains_key(&snapshot.thread_id) {
                let _ = out_tx.send(DaemonMessage::CommentUpdated(snapshot));
            }
        }
        ClientMessage::ReopenComment { comment_id } => {
            let snapshot = sessions.reopen_comment(&comment_id).await?;
            collab.publish_reopen(snapshot.clone());
            if !subscriptions.contains_key(&snapshot.thread_id) {
                let _ = out_tx.send(DaemonMessage::CommentUpdated(snapshot));
            }
        }
        // Detaching is local: the comment stays for everyone, it just does
        // not go out with this person's next send.
        ClientMessage::DetachComment { comment_id: ref id }
        | ClientMessage::AttachComment { comment_id: ref id } => {
            let detach = matches!(msg, ClientMessage::DetachComment { .. });
            let comment_id = id.clone();
            let snapshots = sessions.set_comment_detached(&comment_id, detach).await?;
            for snapshot in snapshots {
                if !subscriptions.contains_key(&snapshot.thread_id) {
                    let _ = out_tx.send(DaemonMessage::CommentUpdated(snapshot));
                }
            }
        }
        ClientMessage::DeleteComment { comment_id } => {
            let (thread_id, comment_ids) = sessions.delete_comment(&comment_id).await?;
            collab.publish_delete(thread_id.clone(), comment_ids.clone());
            if !subscriptions.contains_key(&thread_id) {
                let _ = out_tx.send(DaemonMessage::CommentDeleted {
                    thread_id,
                    comment_ids,
                });
            }
        }
        _ => unreachable!("not a comments message"),
    }
    Ok(ConnectionControl::Continue)
}
