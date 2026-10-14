use super::*;

impl SessionManager {
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceSnapshot>> {
        let workspaces = self.db.list_workspaces().await?;
        let mut out = Vec::with_capacity(workspaces.len());
        for ws in workspaces {
            let repo_count = self.db.list_memberships(&ws.id).await?.len() as u32;
            out.push(WorkspaceSnapshot {
                id: ws.id,
                title: ws.name,
                repo_count,
            });
        }
        Ok(out)
    }

    pub async fn default_workspace_id(&self) -> Result<WorkspaceId> {
        self.list_workspaces()
            .await?
            .into_iter()
            .next()
            .map(|ws| ws.id)
            .ok_or_else(|| DaemonError::NotFound("no workspaces".into()))
    }

    pub async fn list_threads(&self, workspace_id: &WorkspaceId) -> Result<Vec<ThreadSnapshot>> {
        if self.db.get_workspace(workspace_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("workspace {workspace_id}")));
        }
        let threads = self.db.list_threads(workspace_id).await?;
        self.thread_snapshots(&threads).await
    }

    /// Return thread checkout readiness, exposing a path only after validation.
    pub async fn thread_checkouts(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<trama_protocol::CheckoutSnapshot>> {
        use trama_protocol::{CheckoutSnapshot, CheckoutStatus};
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let memberships = match &thread.workspace_id {
            Some(workspace) => self.db.list_memberships(workspace).await?,
            None => Vec::new(),
        };
        // A scout shows (and works in) its parent's checkouts.
        let thread = self.checkout_owner(&thread).await?;
        let thread_id = &thread.id;
        let mut rows = Vec::with_capacity(thread.repo_attachments.len());
        for attachment in thread.repo_attachments.clone() {
            let repository = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::NotFound(format!("repository {}", attachment.repository_id))
                })?;
            let source = repository.local_path.ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {} has no local path",
                    attachment.repository_id
                ))
            })?;
            let source = dunce::canonicalize(&source).unwrap_or(source);
            let removed = match attachment.checkout_id.as_ref() {
                Some(id) => self.db.removed_checkout(id).await?.is_some(),
                None => false,
            };
            let (status, checkout_path, detail) = if removed {
                let detail = if self.db.is_thread_archived(thread_id).await? {
                    archive::ARCHIVED_DETAIL
                } else {
                    archive::REMOVED_DETAIL
                };
                (CheckoutStatus::Removed, None, Some(detail.to_owned()))
            } else if let (Some(_id), Some(base)) = (
                attachment.checkout_id.as_ref(),
                attachment.base_revision.as_ref(),
            ) {
                let resolved = self.checkout_path(thread_id, &attachment, &source).await;
                let (path, repo_local) = match resolved {
                    Ok(resolved) if attachment.attach_mode == AttachMode::Isolated => resolved,
                    other => {
                        let detail = match other {
                            Err(error) => error.to_string(),
                            Ok(_) => "Prepara la copia di lavoro prima di aprirla.".into(),
                        };
                        rows.push(CheckoutSnapshot {
                            repository_id: attachment.repository_id,
                            title: source
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                            source_path: source,
                            checkout_path: None,
                            status: if attachment.attach_mode == AttachMode::Adopted {
                                CheckoutStatus::NeedsPreparation
                            } else {
                                CheckoutStatus::Invalid
                            },
                            detail: Some(detail),
                        });
                        continue;
                    }
                };
                let manager = self.checkout_manager.clone();
                let source_check = source.clone();
                let path_check = path.clone();
                let base_check = base.clone();
                let validation = tokio::task::spawn_blocking(move || {
                    validate_managed_checkout(
                        &manager,
                        &source_check,
                        &path_check,
                        &base_check,
                        repo_local,
                    )
                })
                .await
                .map_err(|e| {
                    DaemonError::Invalid(format!("checkout validation task failed: {e}"))
                })?;
                match validation {
                    Ok(()) => {
                        let detail = trama_infra::process::setup::read_record(&path)
                            .and_then(|record| record.failure_summary());
                        (CheckoutStatus::Ready, Some(path), detail)
                    }
                    Err(e) if !path.exists() => (
                        CheckoutStatus::Missing,
                        None,
                        Some(format!("{e}. Prepare the checkout to continue.")),
                    ),
                    Err(e) => (CheckoutStatus::Invalid, None, Some(e.to_string())),
                }
            } else {
                (
                    CheckoutStatus::NeedsPreparation,
                    None,
                    Some("Prepare this thread's checkout before opening it".into()),
                )
            };
            rows.push(CheckoutSnapshot {
                repository_id: attachment.repository_id.clone(),
                title: memberships
                    .iter()
                    .find(|membership| membership.repository_id == attachment.repository_id)
                    .and_then(|membership| membership.alias.clone())
                    .unwrap_or_else(|| {
                        source
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }),
                source_path: source,
                checkout_path,
                status,
                detail,
            });
        }
        Ok(rows)
    }

    /// A recorded local path is authoritative; absence means a legacy global
    /// checkout, never permission to guess or create a replacement.
    pub(super) async fn checkout_path(
        &self,
        thread: &ThreadId,
        attachment: &ThreadRepositoryAttachment,
        source: &std::path::Path,
    ) -> Result<(PathBuf, bool)> {
        let id = attachment.checkout_id.as_ref().ok_or_else(|| {
            DaemonError::Invalid("la copia di lavoro deve essere preparata".into())
        })?;
        if let Some(path) = self
            .db
            .checkout_location(thread, &attachment.repository_id, id)
            .await?
        {
            let expected = self
                .checkout_manager
                .repo_local_path(source, thread, id)
                .map_err(checkout_to_invalid)?;
            if path != expected {
                return Err(DaemonError::Invalid(
                    "la posizione registrata non corrisponde a questa copia di lavoro".into(),
                ));
            }
            Ok((path, true))
        } else {
            Ok((
                self.checkout_manager
                    .path(thread, &attachment.repository_id, id)
                    .map_err(checkout_to_invalid)?,
                false,
            ))
        }
    }

    pub async fn create_thread(
        &self,
        workspace_id: WorkspaceId,
        title: String,
    ) -> Result<ThreadSnapshot> {
        let workspace = self
            .db
            .get_workspace(&workspace_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("workspace {workspace_id}")))?;
        let memberships = self.db.list_memberships(&workspace_id).await?;
        if !memberships
            .iter()
            .any(|membership| membership.default_enabled)
        {
            return Err(DaemonError::Invalid(
                "workspace has no default repository. Add a local Git repository before creating a thread"
                    .into(),
            ));
        }
        // Build attachments in the same order the database reloads them
        // (repository id ascending) so the cwd chosen from the first
        // attachment at creation is identical after a restart. The order is
        // derived from persisted ids, not from aliases or insertion order.
        let mut memberships = memberships;
        memberships.sort_by(|a, b| a.repository_id.0.cmp(&b.repository_id.0));
        let mut thread = Thread::new_for_workspace(&workspace, &memberships, Some(title));
        let mut provision_requests = Vec::with_capacity(thread.repo_attachments.len());
        for attachment in &mut thread.repo_attachments {
            let repository = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::NotFound(format!("repository {}", attachment.repository_id))
                })?;
            let source = repository.local_path.ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {} has no local path",
                    attachment.repository_id
                ))
            })?;
            let checkout_id = CheckoutId::generate();
            let alias = memberships
                .iter()
                .find(|membership| membership.repository_id == attachment.repository_id)
                .and_then(|membership| membership.alias.clone());
            provision_requests.push((source, attachment.repository_id.clone(), checkout_id, alias));
        }
        let checkout_manager = self.checkout_manager.clone();
        let thread_id = thread.id.clone();
        let thread_title = thread.title.clone().unwrap_or_default();
        let provisioned = tokio::task::spawn_blocking(move || {
            provision_requests
                .into_iter()
                .map(|(source, repository_id, checkout_id, alias)| {
                    checkout_manager
                        .provision_repo_local(&source, &thread_id, &repository_id, &checkout_id)
                        .map(|checkout| {
                            // Project setup; a failure is recorded on the
                            // checkout and never fails thread creation.
                            run_checkout_setup(
                                &source,
                                &checkout.path,
                                &thread_id,
                                &thread_title,
                                alias.as_deref(),
                            );
                            (
                                CheckoutLocation {
                                    checkout_id,
                                    repository_id,
                                    path: checkout.path,
                                },
                                checkout.base_revision,
                            )
                        })
                })
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .await
        .map_err(|error| {
            DaemonError::Invalid(format!("checkout provisioning task failed: {error}"))
        })?
        .map_err(checkout_to_invalid)?;
        let mut locations = Vec::with_capacity(provisioned.len());
        for (attachment, (location, base_revision)) in
            thread.repo_attachments.iter_mut().zip(provisioned)
        {
            attachment.checkout_id = Some(location.checkout_id.clone());
            attachment.base_revision = Some(base_revision);
            locations.push(location);
        }
        self.db
            .create_thread_with_locations(&thread, &locations)
            .await?;
        let record = EventRecord::new(
            Aggregate::Thread {
                id: thread.id.clone(),
            },
            Actor::System,
            DomainEvent::ThreadCreated {
                thread: thread.clone(),
            },
            vec![],
        )?;
        EventStore::append(&self.db, vec![record]).await?;
        Ok(thread_snapshot(&thread))
    }

    pub async fn create_workspace(&self, name: String) -> Result<WorkspaceSnapshot> {
        let name = validate_workspace_name(&name)?;
        if self.db.get_workspace_by_name(&name).await?.is_some() {
            return Err(DaemonError::Invalid(format!(
                "a workspace named `{name}` already exists"
            )));
        }
        let workspace = Workspace::new(name);
        let record = EventRecord::new(
            Aggregate::Workspace {
                id: workspace.id.clone(),
            },
            Actor::System,
            DomainEvent::WorkspaceCreated {
                workspace: workspace.clone(),
            },
            vec![],
        )?;
        self.db
            .create_workspace_with_event(&workspace, &record)
            .await?;
        Ok(WorkspaceSnapshot {
            id: workspace.id,
            title: workspace.name,
            repo_count: 0,
        })
    }

    pub async fn add_repository(
        &self,
        workspace_id: WorkspaceId,
        path: PathBuf,
        alias: String,
    ) -> Result<RepositorySnapshot> {
        if self.db.get_workspace(&workspace_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("workspace {workspace_id}")));
        }
        let alias = validate_label(&alias, "repository alias")?;
        let inspected =
            CheckoutManager::inspect_working_root(&path).map_err(checkout_to_invalid)?;
        let candidate = Repository::new(
            inspected.origin,
            Some(inspected.path.clone()),
            inspected.default_branch,
        );
        let stored = self
            .db
            .upsert_repository_membership(
                &candidate,
                &WorkspaceMembership {
                    workspace_id,
                    repository_id: candidate.id.clone(),
                    alias: Some(alias.clone()),
                    default_enabled: true,
                },
            )
            .await?;
        Ok(repository_snapshot(&stored, &alias, true))
    }

    pub async fn list_repositories(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<RepositorySnapshot>> {
        if self.db.get_workspace(workspace_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("workspace {workspace_id}")));
        }
        let memberships = self.db.list_memberships(workspace_id).await?;
        let mut out = Vec::with_capacity(memberships.len());
        for membership in memberships {
            let repository = self
                .db
                .get_repository(&membership.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::NotFound(format!("repository {}", membership.repository_id))
                })?;
            let title = membership.alias.clone().unwrap_or_else(|| {
                repository
                    .local_path
                    .as_ref()
                    .and_then(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| repository.id.to_string())
            });
            out.push(repository_snapshot(
                &repository,
                &title,
                membership.default_enabled,
            ));
        }
        Ok(out)
    }

    /// Explicit migration. Reserves the thread for the whole operation so a
    /// concurrent prompt or migration is rejected (`busy`), prepares isolated
    /// clones, then atomically records checkout metadata, invalidates the
    /// previous external agent sessions, and appends a system message. Does
    /// not start the adapter and does not modify source working trees.
    pub async fn migrate_legacy_thread(&self, thread_id: ThreadId) -> Result<ThreadSnapshot> {
        let thread = self
            .db
            .get_thread(&thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let _migration = self.begin_migration(&thread_id)?;
        if thread.repo_attachments.is_empty() {
            return Err(DaemonError::Invalid(
                "thread has no repository attachments to migrate".into(),
            ));
        }

        let mut plans = Vec::with_capacity(thread.repo_attachments.len());
        for attachment in &thread.repo_attachments {
            let repository = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::Invalid(format!(
                        "repository {} is missing from the database. The thread was not changed",
                        attachment.repository_id
                    ))
                })?;
            let source = repository.local_path.clone().ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {} has no local path. Restore the Git working tree before migrating; nothing was changed",
                    attachment.repository_id
                ))
            })?;
            if !source.exists() {
                return Err(DaemonError::Invalid(format!(
                    "repository {} is missing at {}. Restore the local Git working tree before migrating; the thread and its messages were not changed",
                    attachment.repository_id,
                    source.display()
                )));
            }
            let inspected =
                CheckoutManager::inspect_working_root(&source).map_err(migration_checkout_error)?;
            let managed = attachment.attach_mode == AttachMode::Isolated
                && attachment.checkout_id.is_some()
                && attachment.base_revision.is_some();
            if managed {
                let (checkout_path, repo_local) =
                    self.checkout_path(&thread_id, attachment, &source).await?;
                plans.push(MigrationPlan {
                    source: inspected.path,
                    attachment: attachment.clone(),
                    provision: false,
                    checkout_path: Some(checkout_path),
                    repo_local,
                });
            } else {
                plans.push(MigrationPlan {
                    source: inspected.path,
                    attachment: ThreadRepositoryAttachment {
                        repository_id: attachment.repository_id.clone(),
                        base_revision: None,
                        checkout_id: Some(CheckoutId::generate()),
                        attach_mode: AttachMode::Isolated,
                    },
                    provision: true,
                    checkout_path: None,
                    repo_local: true,
                });
            }
        }

        let changed = plans.iter().any(|plan| plan.provision);
        let checkout_manager = self.checkout_manager.clone();
        let thread_key = thread.id.clone();
        let plans = tokio::task::spawn_blocking(move || {
            prepare_migration(checkout_manager, &thread_key, plans)
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("checkout migration task failed: {error}")))?
        .map_err(migration_checkout_error)?;

        if !changed {
            return Ok(thread_snapshot(&thread));
        }

        let attachments: Vec<_> = plans.iter().map(|plan| plan.attachment.clone()).collect();
        let locations: Vec<_> = plans
            .iter()
            .filter(|plan| plan.repo_local)
            .filter_map(|plan| {
                Some(CheckoutLocation {
                    checkout_id: plan.attachment.checkout_id.clone()?,
                    repository_id: plan.attachment.repository_id.clone(),
                    path: plan.checkout_path.clone()?,
                })
            })
            .collect();
        if attachments.iter().any(|attachment| {
            attachment.checkout_id.is_none() || attachment.base_revision.is_none()
        }) {
            return Err(DaemonError::Invalid(
                "migration did not record a checkout. The thread was not changed".into(),
            ));
        }
        let message = Message {
            id: MessageId::generate(),
            thread_id: thread.id.clone(),
            parent_id: None,
            actor: Actor::System,
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: MIGRATION_NOTICE.into(),
            }],
            created_at: Utc::now(),
            claude_uuid: None,
            annotation: None,
        };
        let event = EventRecord::new(
            Aggregate::Thread {
                id: thread.id.clone(),
            },
            Actor::System,
            DomainEvent::MessageAdded {
                message: message.clone(),
            },
            vec![],
        )?;
        self.db
            .persist_thread_migration_with_locations(
                &thread.id,
                &attachments,
                &message,
                &event,
                &locations,
            )
            .await?;
        self.broadcast(
            &thread.id,
            DaemonMessage::MessageAppended {
                thread_id: thread.id.clone(),
                message,
            },
        );
        let mut updated = thread;
        updated.repo_attachments = attachments;
        Ok(thread_snapshot(&updated))
    }
}
