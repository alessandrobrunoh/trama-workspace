use super::*;

pub(super) async fn handle_line(
    line: &str,
    sessions: &SessionManager,
    collab: &CollabService,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    subscriptions: &mut HashMap<ThreadId, SubscriptionId>,
    shutdown: &watch::Sender<bool>,
) -> Result<ConnectionControl> {
    let msg: ClientMessage = match decode_json_line(line) {
        Ok(msg) => msg,
        Err(err) => {
            let _ = out_tx.send(DaemonMessage::Error {
                code: "invalid_json".into(),
                message: err.to_string(),
            });
            return Ok(ConnectionControl::Continue);
        }
    };

    match msg {
        ClientMessage::Hello { .. } => {
            let _ = out_tx.send(DaemonMessage::Error {
                code: "protocol".into(),
                message: "hello already accepted".into(),
            });
        }
        ClientMessage::ListWorkspaces => {
            let _ = out_tx.send(DaemonMessage::Workspaces(sessions.list_workspaces().await?));
        }
        ClientMessage::ListThreads { workspace_id } => {
            let threads = sessions.list_threads(&workspace_id).await?;
            let _ = out_tx.send(DaemonMessage::Threads(threads));
        }
        ClientMessage::CreateThread {
            workspace_id,
            title,
        } => {
            let snapshot = sessions.create_thread(workspace_id, title).await?;
            let _ = out_tx.send(DaemonMessage::ThreadCreated(snapshot));
        }
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
        ClientMessage::CreateWorkspace { name } => {
            let snapshot = sessions.create_workspace(name).await?;
            let _ = out_tx.send(DaemonMessage::WorkspaceCreated(snapshot));
        }
        ClientMessage::AddRepository {
            workspace_id,
            path,
            alias,
        } => {
            let repository = sessions
                .add_repository(workspace_id.clone(), path, alias)
                .await?;
            let _ = out_tx.send(DaemonMessage::RepositoryAdded {
                workspace_id,
                repository,
            });
        }
        ClientMessage::ListRepositories { workspace_id } => {
            let repositories = sessions.list_repositories(&workspace_id).await?;
            let _ = out_tx.send(DaemonMessage::Repositories {
                workspace_id,
                repositories,
            });
        }
        ClientMessage::MigrateLegacyThread { thread_id } => {
            let snapshot = sessions.migrate_legacy_thread(thread_id).await?;
            let _ = out_tx.send(DaemonMessage::ThreadMigrated(snapshot));
        }
        ClientMessage::GetThreadCheckouts { thread_id } => {
            let checkouts = sessions.thread_checkouts(&thread_id).await?;
            let _ = out_tx.send(DaemonMessage::ThreadCheckouts {
                thread_id,
                checkouts,
            });
        }
        ClientMessage::ApprovalHook { .. } => {
            // Intercepted by `handle_connection` before reaching here.
            let _ = out_tx.send(DaemonMessage::Error {
                code: "protocol".into(),
                message: "approval_hook is only accepted from the approval hook".into(),
            });
        }
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
        ClientMessage::ForkThread {
            thread_id,
            at_message_id,
            title,
            initial_prompt,
            kind,
        } => {
            let snapshot = sessions
                .fork_thread(thread_id, at_message_id, title, initial_prompt, kind)
                .await?;
            let _ = out_tx.send(DaemonMessage::SubthreadCreated(snapshot));
        }
        ClientMessage::McpToolCall { .. } => {
            // Intercepted by `handle_connection` before reaching here.
            let _ = out_tx.send(DaemonMessage::Error {
                code: "protocol".into(),
                message: "mcp_tool_call is only accepted from the Trama MCP helper".into(),
            });
        }
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
                items: crate::session::model_choices(),
            });
        }
        // Terminal events go to the thread's subscribers; a requester that
        // is not subscribed gets the opening reply directly.
        ClientMessage::OpenTerminal {
            thread_id,
            repository_id,
            command,
            cols,
            rows,
        } => {
            let terminal = sessions
                .open_terminal(&thread_id, repository_id, command, cols, rows)
                .await?;
            if !subscriptions.contains_key(&thread_id) {
                let _ = out_tx.send(DaemonMessage::TerminalOpened {
                    thread_id,
                    terminal,
                });
            }
        }
        ClientMessage::TerminalInput { terminal_id, data } => {
            sessions.terminal_input(&terminal_id, data).await?;
        }
        ClientMessage::ResizeTerminal {
            terminal_id,
            cols,
            rows,
        } => {
            sessions.resize_terminal(&terminal_id, cols, rows)?;
        }
        ClientMessage::CloseTerminal { terminal_id } => {
            sessions.close_terminal(&terminal_id)?;
        }
        ClientMessage::ListTerminals { thread_id } => {
            let terminals = sessions.list_terminals(&thread_id);
            let _ = out_tx.send(DaemonMessage::Terminals {
                thread_id,
                terminals,
            });
        }
        ClientMessage::ReplayTerminal { terminal_id } => {
            let _ = out_tx.send(sessions.replay_terminal(&terminal_id)?);
        }
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
        ClientMessage::GetDiskUsage { workspace_id } => {
            let items = sessions.disk_usage(workspace_id.as_ref()).await?;
            let _ = out_tx.send(DaemonMessage::DiskUsage {
                workspace_id,
                items,
            });
        }
        ClientMessage::ArchiveThread {
            thread_id,
            delete_checkouts,
            force,
        } => {
            let reply = match sessions
                .archive_thread(&thread_id, delete_checkouts, force)
                .await?
            {
                Ok((thread, removed)) => DaemonMessage::ThreadArchived { thread, removed },
                Err(checkouts) => DaemonMessage::CheckoutRemovalRefused {
                    thread_id,
                    checkouts,
                },
            };
            let _ = out_tx.send(reply);
        }
        ClientMessage::UnarchiveThread { thread_id } => {
            let thread = sessions.unarchive_thread(&thread_id).await?;
            let _ = out_tx.send(DaemonMessage::ThreadUnarchived(thread));
        }
        ClientMessage::DeleteThreadCheckouts { thread_id, force } => {
            let reply = match sessions.delete_thread_checkouts(&thread_id, force).await? {
                crate::session::RemovalOutcome::Removed(removed) => {
                    DaemonMessage::ThreadCheckoutsDeleted { thread_id, removed }
                }
                crate::session::RemovalOutcome::Refused(checkouts) => {
                    DaemonMessage::CheckoutRemovalRefused {
                        thread_id,
                        checkouts,
                    }
                }
            };
            let _ = out_tx.send(reply);
        }
        ClientMessage::GetRemoteChanges { thread_id } => {
            let snapshots = collab.remote_changes(&thread_id).await?;
            let _ = out_tx.send(DaemonMessage::RemoteChanges {
                thread_id,
                snapshots,
            });
        }
        ClientMessage::Shutdown => {
            let _ = shutdown.send(true);
            return Ok(ConnectionControl::Close);
        }
    }
    Ok(ConnectionControl::Continue)
}
