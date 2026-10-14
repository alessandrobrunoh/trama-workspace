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
        ClientMessage::SubscribeThread { thread_id } => {
            if subscriptions.contains_key(&thread_id) {
                return Ok(ConnectionControl::Continue);
            }
            let sub_id = sessions.subscribe(&thread_id, out_tx.clone()).await?;
            subscriptions.insert(thread_id, sub_id);
        }
        ClientMessage::UnsubscribeThread { thread_id } => {
            if let Some(sub_id) = subscriptions.remove(&thread_id) {
                sessions.unsubscribe(&thread_id, sub_id)?;
            }
        }
        ClientMessage::SendPrompt {
            thread_id,
            text,
            mentions,
            terminals,
        } => {
            // A shared thread's turn goes through the collab service; this
            // daemon's runner claims it back and runs it here (ADR 0008).
            // Mentions and terminal attachments only apply to local turns.
            let sent = match collab.link(&thread_id).await {
                Ok(Some(link)) => collab.submit_turn(&link, text).await.map(|command_id| {
                    let _ = out_tx.send(DaemonMessage::CollabTurnQueued {
                        thread_id: thread_id.clone(),
                        command_id,
                    });
                }),
                Ok(None) => {
                    let context = crate::session::PromptContext {
                        mentions,
                        terminals,
                    };
                    sessions
                        .send_prompt_with_context(thread_id.clone(), text, context)
                        .await
                        .map(|_| ())
                }
                Err(error) => Err(error),
            };
            if let Err(error) = sent {
                let message = error.to_string();
                let _ = out_tx.send(DaemonMessage::Error {
                    code: error.code().into(),
                    message: message.clone(),
                });
                // This attempt was rejected, not another subscriber's running
                // turn. Reply only to the requesting client, scoped by thread.
                let _ = out_tx.send(DaemonMessage::AgentEventEnvelope {
                    thread_id,
                    event: trama_domain::agent::AgentEvent::Failed { message },
                });
            }
        }
        ClientMessage::Interrupt { thread_id } => {
            sessions.interrupt(&thread_id).await?;
        }
        ClientMessage::RespondApproval {
            thread_id,
            request_id,
            decision,
        } => {
            sessions
                .respond_approval(&thread_id, &request_id, decision)
                .await?;
        }
        ClientMessage::ListMentionables {
            thread_id,
            query,
            limit,
        } => {
            let items = sessions
                .list_mentionables(&thread_id, &query, limit)
                .await?;
            let _ = out_tx.send(DaemonMessage::Mentionables {
                thread_id,
                query,
                items,
            });
        }
        ClientMessage::ListSkills { thread_id } => {
            let items = sessions.list_skills(&thread_id).await?;
            let _ = out_tx.send(DaemonMessage::Skills { thread_id, items });
        }
        ClientMessage::SetThreadModel { thread_id, model } => {
            let model = sessions.set_thread_model(&thread_id, model).await?;
            if !subscriptions.contains_key(&thread_id) {
                let _ = out_tx.send(DaemonMessage::ThreadModelChanged { thread_id, model });
            }
        }
        ClientMessage::ListModels => {
            let _ = out_tx.send(DaemonMessage::Models {
                items: sessions.all_model_choices(),
            });
        }
        // Terminal events go to the thread's subscribers; a requester that
        // is not subscribed gets the opening reply directly.
        _ => unreachable!("not a turns message"),
    }
    Ok(ConnectionControl::Continue)
}
