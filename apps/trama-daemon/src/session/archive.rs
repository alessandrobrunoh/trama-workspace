//! Disk usage of thread checkouts, archiving threads, and removing their
//! checkouts to free disk (ADR 0012).
//!
//! Checkouts are temporary. Removing one first saves its whole state
//! (commits the source lacks, uncommitted and untracked files) to an archive
//! bundle under the app data directory; the checkout is recreated at its
//! recorded path from a copy-on-write clone of the source plus that bundle
//! when the thread is unarchived or next needs it (a turn, a fork, pulling
//! changes). Read-only views of a removed checkout fail instead of
//! recreating it.

use super::*;
use std::path::Path;
use trama_db::ThreadCheckoutRow;
use trama_domain::ids::RepositoryId;
use trama_domain::subagent::SubagentProfile;
use trama_infra::git::checkout::RemovalReport;
use trama_protocol::{CheckoutRemoval, DiskUsageItem};

/// Serializes checkout recreation, so two requests cannot restore the same
/// checkout at once.
static RESTORES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) const REMOVED_DETAIL: &str =
    "Removed to free disk; recreated from its saved state the next time the thread needs it";
pub(crate) const ARCHIVED_DETAIL: &str =
    "Removed when the thread was archived; unarchive the thread to recreate it";

/// Outcome of removing a thread's checkouts.
pub enum RemovalOutcome {
    Removed(Vec<CheckoutRemoval>),
    /// Work exists only in these checkouts and `force` was not set; nothing
    /// was changed.
    Refused(Vec<CheckoutRemoval>),
}

/// One checkout of the thread being removed.
struct Target {
    row: ThreadCheckoutRow,
    source: PathBuf,
    title: String,
}

impl SessionManager {
    /// Disk used by the checkouts present on disk (of `workspace_id`, or
    /// all), largest private size first.
    pub async fn disk_usage(
        &self,
        workspace_id: Option<&WorkspaceId>,
    ) -> Result<Vec<DiskUsageItem>> {
        let rows = self.db.list_thread_checkouts(workspace_id).await?;
        let archived = self.db.archived_thread_ids().await?;
        let mut items = Vec::new();
        for row in rows {
            if row.removed_snapshot.is_some() {
                continue;
            }
            let Some(thread) = self.db.get_thread(&row.thread_id).await? else {
                continue;
            };
            let repository_title = self.repository_title(&thread, &row.repository_id).await?;
            items.push(DiskUsageItem {
                thread_title: thread.title.clone().unwrap_or_default(),
                thread_id: row.thread_id.clone(),
                repository_id: row.repository_id.clone(),
                repository_title,
                path: row.path.clone(),
                apparent_bytes: 0,
                private_bytes: 0,
                private_estimated: false,
                last_active: self.db.thread_last_active(&row.thread_id).await?,
                archived: archived.contains(&row.thread_id),
            });
        }
        let mut items = tokio::task::spawn_blocking(move || {
            items.retain(|item| item.path.is_dir());
            for item in &mut items {
                match trama_infra::fs::cow::disk_usage(&item.path) {
                    Ok(usage) => {
                        item.apparent_bytes = usage.apparent_bytes;
                        item.private_bytes = usage.private_bytes;
                        item.private_estimated = usage.estimated;
                    }
                    Err(_) => item.private_estimated = true,
                }
            }
            items
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("disk usage task failed: {error}")))?;
        items.sort_by(|a, b| {
            b.private_bytes
                .cmp(&a.private_bytes)
                .then(b.apparent_bytes.cmp(&a.apparent_bytes))
        });
        Ok(items)
    }

    /// Archive a thread, removing its checkouts when `delete_checkouts`.
    /// `Ok(Err(..))` is a refusal: unsaved work and no `force`.
    pub async fn archive_thread(
        &self,
        thread_id: &ThreadId,
        delete_checkouts: bool,
        force: bool,
    ) -> Result<std::result::Result<(ThreadSnapshot, Vec<CheckoutRemoval>), Vec<CheckoutRemoval>>>
    {
        let thread = self.thread(thread_id).await?;
        let removed = if delete_checkouts {
            match self.remove_thread_checkouts(&thread, force).await? {
                RemovalOutcome::Removed(removed) => removed,
                RemovalOutcome::Refused(checkouts) => return Ok(Err(checkouts)),
            }
        } else {
            // Stop the agent; the files stay.
            let _guard = self.reserve_checkouts(std::slice::from_ref(thread_id))?;
            self.stop_thread_processes(thread_id).await;
            Vec::new()
        };
        self.db.set_thread_archived(thread_id, true).await?;
        let snapshot = self.thread_snapshots(&[thread]).await?.remove(0);
        Ok(Ok((snapshot, removed)))
    }

    /// Bring an archived thread back, recreating its removed checkouts first
    /// (the thread stays archived if that fails).
    pub async fn unarchive_thread(&self, thread_id: &ThreadId) -> Result<ThreadSnapshot> {
        let thread = self.thread(thread_id).await?;
        self.restore_removed_checkouts(&thread).await?;
        self.db.set_thread_archived(thread_id, false).await?;
        Ok(self.thread_snapshots(&[thread]).await?.remove(0))
    }

    /// Remove a thread's checkouts without archiving it.
    pub async fn delete_thread_checkouts(
        &self,
        thread_id: &ThreadId,
        force: bool,
    ) -> Result<RemovalOutcome> {
        let thread = self.thread(thread_id).await?;
        self.remove_thread_checkouts(&thread, force).await
    }

    /// Recreate removed checkouts of `owner` before work needs them. Fails
    /// for an archived thread.
    pub(super) async fn ensure_checkouts_present(&self, owner: &Thread) -> Result<()> {
        if !self.has_removed_checkouts(&owner.id).await? {
            return Ok(());
        }
        if self.db.is_thread_archived(&owner.id).await? {
            return Err(DaemonError::Invalid(format!(
                "thread {} is archived and its checkouts were removed. Unarchive it to recreate them",
                owner.id
            )));
        }
        self.restore_removed_checkouts(owner).await
    }

    /// Error for read-only access to a thread whose checkouts were removed.
    pub(super) async fn reject_removed_checkouts(&self, owner: &Thread) -> Result<()> {
        if self.has_removed_checkouts(&owner.id).await? {
            let detail = if self.db.is_thread_archived(&owner.id).await? {
                ARCHIVED_DETAIL
            } else {
                REMOVED_DETAIL
            };
            return Err(DaemonError::Invalid(format!(
                "the checkouts of thread {} are not on disk. {detail}",
                owner.id
            )));
        }
        Ok(())
    }

    async fn has_removed_checkouts(&self, thread_id: &ThreadId) -> Result<bool> {
        Ok(self
            .owned_checkouts(thread_id)
            .await?
            .iter()
            .any(|row| row.removed_snapshot.is_some()))
    }

    async fn thread(&self, thread_id: &ThreadId) -> Result<Thread> {
        self.db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))
    }

    /// Checkout locations recorded for `thread_id` itself.
    async fn owned_checkouts(&self, thread_id: &ThreadId) -> Result<Vec<ThreadCheckoutRow>> {
        let thread = self.thread(thread_id).await?;
        let rows = self
            .db
            .list_thread_checkouts(thread.workspace_id.as_ref())
            .await?;
        Ok(rows
            .into_iter()
            .filter(|row| &row.thread_id == thread_id)
            .collect())
    }

    async fn repository_title(
        &self,
        thread: &Thread,
        repository_id: &RepositoryId,
    ) -> Result<String> {
        if let Some(workspace) = &thread.workspace_id {
            if let Some(alias) = self
                .db
                .list_memberships(workspace)
                .await?
                .into_iter()
                .find(|membership| &membership.repository_id == repository_id)
                .and_then(|membership| membership.alias)
            {
                return Ok(alias);
            }
        }
        Ok(self
            .db
            .get_repository(repository_id)
            .await?
            .and_then(|repository| repository.local_path)
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| repository_id.to_string()))
    }

    async fn source_of(&self, repository_id: &RepositoryId) -> Result<PathBuf> {
        self.db
            .get_repository(repository_id)
            .await?
            .and_then(|repository| repository.local_path)
            .ok_or_else(|| {
                DaemonError::Invalid(format!("repository {repository_id} has no local path"))
            })
    }

    /// End the thread's agent session and terminals (their working
    /// directories are about to disappear).
    async fn stop_thread_processes(&self, thread_id: &ThreadId) {
        let session = self
            .lock()
            .threads
            .get(thread_id)
            .and_then(|thread| thread.live_session.clone());
        if let Some(session) = session {
            self.retire_live_session(thread_id, session).await;
        }
        self.close_thread_terminals(thread_id);
    }

    /// Save, then delete, the checkouts `thread` owns. Refused while a turn,
    /// fork or pull uses the thread (or a scout working in its checkouts).
    async fn remove_thread_checkouts(
        &self,
        thread: &Thread,
        force: bool,
    ) -> Result<RemovalOutcome> {
        let mut users = vec![thread.id.clone()];
        for child in self.db.list_subthreads(&thread.id).await? {
            let scout = self
                .db
                .get_subagent(&child.id)
                .await?
                .is_some_and(|record| record.spawned_by.profile == SubagentProfile::Scout);
            if scout {
                users.push(child.id);
            }
        }
        let _guard = self.reserve_checkouts(&users)?;
        let mut targets = Vec::new();
        for row in self.owned_checkouts(&thread.id).await? {
            if row.removed_snapshot.is_some() {
                continue;
            }
            let source = self.source_of(&row.repository_id).await?;
            let title = self.repository_title(thread, &row.repository_id).await?;
            targets.push(Target { row, source, title });
        }
        let manager = self.checkout_manager.clone();
        let (targets, reports) = tokio::task::spawn_blocking(move || {
            let mut reports = Vec::with_capacity(targets.len());
            for target in &targets {
                let report = if target.row.path.exists() {
                    manager.removal_report(&target.source, &target.row.path)?
                } else {
                    RemovalReport::default()
                };
                let freed = trama_infra::fs::cow::disk_usage(&target.row.path)
                    .ok()
                    .map(|usage| usage.private_bytes);
                reports.push(removal(target, &report, freed));
            }
            Ok::<_, CheckoutError>((targets, reports))
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("checkout inspection failed: {error}")))?
        .map_err(checkout_to_invalid)?;
        if !force && reports.iter().any(CheckoutRemoval::has_unsaved_work) {
            return Ok(RemovalOutcome::Refused(reports));
        }
        for user in &users {
            self.stop_thread_processes(user).await;
        }
        // Save every checkout before deleting any.
        let manager = self.checkout_manager.clone();
        let thread_id = thread.id.clone();
        let (targets, snapshots) = tokio::task::spawn_blocking(move || {
            let mut snapshots = Vec::with_capacity(targets.len());
            for target in &targets {
                let snapshot = if target.row.path.exists() {
                    Some(
                        manager
                            .archive_repo_local(
                                &target.source,
                                &thread_id,
                                &target.row.checkout_id,
                                &target.row.path,
                            )?
                            .snapshot,
                    )
                } else {
                    None
                };
                snapshots.push(snapshot);
            }
            Ok::<_, CheckoutError>((targets, snapshots))
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("checkout archive failed: {error}")))?
        .map_err(checkout_to_invalid)?;
        for (target, snapshot) in targets.iter().zip(&snapshots) {
            let Some(snapshot) = snapshot else {
                continue;
            };
            // Recorded first: a checkout left half-deleted is still known
            // to be removed and is recreated from its archive.
            self.db
                .record_removed_checkout(&thread.id, &target.row.checkout_id, snapshot)
                .await?;
            let manager = self.checkout_manager.clone();
            let source = target.source.clone();
            let path = target.row.path.clone();
            tokio::task::spawn_blocking(move || manager.remove_repo_local(&source, &path))
                .await
                .map_err(|error| DaemonError::Invalid(format!("checkout removal failed: {error}")))?
                .map_err(checkout_to_invalid)?;
        }
        Ok(RemovalOutcome::Removed(reports))
    }

    /// Recreate every removed checkout of `thread` from its archive and run
    /// project setup in it.
    async fn restore_removed_checkouts(&self, thread: &Thread) -> Result<()> {
        let _serial = RESTORES.lock().await;
        let memberships = match &thread.workspace_id {
            Some(workspace) => self.db.list_memberships(workspace).await?,
            None => Vec::new(),
        };
        for row in self.owned_checkouts(&thread.id).await? {
            let Some(snapshot) = row.removed_snapshot.clone() else {
                continue;
            };
            let source = self.source_of(&row.repository_id).await?;
            let alias = memberships
                .iter()
                .find(|membership| membership.repository_id == row.repository_id)
                .and_then(|membership| membership.alias.clone());
            let manager = self.checkout_manager.clone();
            let thread_id = thread.id.clone();
            let title = thread.title.clone().unwrap_or_default();
            let checkout_id = row.checkout_id.clone();
            let repository_id = row.repository_id.clone();
            let expected = row.path.clone();
            tokio::task::spawn_blocking(move || {
                let archived = manager.read_archive(&thread_id, &checkout_id)?;
                if archived.snapshot != snapshot {
                    return Err(CheckoutError::Invalid(archived.bundle));
                }
                if fs_exists(&expected) {
                    // Left over from an interrupted removal.
                    manager.remove_repo_local(&source, &expected)?;
                }
                let path = manager.restore_repo_local(
                    &source,
                    &thread_id,
                    &repository_id,
                    &checkout_id,
                    &archived,
                )?;
                if path != expected {
                    return Err(CheckoutError::Containment(path));
                }
                run_checkout_setup(&source, &path, &thread_id, &title, alias.as_deref());
                manager.discard_archive(&archived);
                Ok(())
            })
            .await
            .map_err(|error| DaemonError::Invalid(format!("checkout restore failed: {error}")))?
            .map_err(|error| {
                DaemonError::Invalid(format!(
                    "could not recreate the checkout at {}: {error}. Its saved state is kept",
                    row.path.display()
                ))
            })?;
            self.db.clear_removed_checkout(&row.checkout_id).await?;
        }
        Ok(())
    }
}

fn fs_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn removal(target: &Target, report: &RemovalReport, freed: Option<u64>) -> CheckoutRemoval {
    CheckoutRemoval {
        repository_id: target.row.repository_id.clone(),
        title: target.title.clone(),
        path: target.row.path.clone(),
        changed_files: report.changed_files.clone(),
        changed_file_count: u32::try_from(report.changed_file_count).unwrap_or(u32::MAX),
        commits_not_in_source: u32::try_from(report.commits_not_in_source).unwrap_or(u32::MAX),
        freed_bytes: freed,
    }
}
