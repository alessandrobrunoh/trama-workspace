use super::*;

pub(super) async fn handle(msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let Ctx {
        sessions, out_tx, ..
    } = ctx;
    match msg {
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
        _ => unreachable!("not a workspaces message"),
    }
    Ok(ConnectionControl::Continue)
}
