use super::*;

pub(super) async fn handle(msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let Ctx {
        sessions,
        collab,
        out_tx,
        ..
    } = ctx;
    match msg {
        ClientMessage::CreateSubthread {
            parent_thread_id,
            kind,
            title,
            initial_prompt,
        } => {
            let snapshot = sessions
                .create_subthread(parent_thread_id, kind, title, initial_prompt)
                .await?;
            let _ = out_tx.send(DaemonMessage::SubthreadCreated(snapshot));
        }
        ClientMessage::ListSubthreads { parent_thread_id } => {
            let threads = sessions.list_subthreads(&parent_thread_id).await?;
            let _ = out_tx.send(DaemonMessage::Subthreads {
                parent_thread_id,
                threads,
            });
        }
        ClientMessage::GetChanges { thread_id, base } => {
            let repos = sessions.get_changes(&thread_id, base).await?;
            let _ = out_tx.send(DaemonMessage::Changes {
                thread_id,
                base,
                repos,
            });
        }
        ClientMessage::PullChanges { subthread_id } => {
            let (parent_thread_id, repos) = sessions.pull_changes(&subthread_id).await?;
            let _ = out_tx.send(DaemonMessage::ChangesPulled {
                subthread_id,
                parent_thread_id,
                repos,
            });
        }
        ClientMessage::SubmitReviewVerdict {
            review_thread_id,
            verdict,
        } => {
            let parent_thread_id = sessions
                .submit_review_verdict(&review_thread_id, verdict.clone())
                .await?;
            collab.publish_verdict(review_thread_id.clone());
            let _ = out_tx.send(DaemonMessage::ReviewVerdictPosted {
                review_thread_id,
                parent_thread_id,
                verdict,
            });
        }
        ClientMessage::GetRemoteChanges { thread_id } => {
            let snapshots = collab.remote_changes(&thread_id).await?;
            let _ = out_tx.send(DaemonMessage::RemoteChanges {
                thread_id,
                snapshots,
            });
        }
        _ => unreachable!("not a subthreads message"),
    }
    Ok(ConnectionControl::Continue)
}
