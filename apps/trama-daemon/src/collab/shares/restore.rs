//! Joiner restore of shared checkouts (docs/plan/04 §restore).
//!
//! Each repository of a share is kept in a staging repository (the
//! *source*) under `<app dir>/share-checkouts/<16 hex>/`, filled from the
//! `origin_hint` with the user's own Git credentials when there is one,
//! otherwise from the bundle alone; `share_git::restore` verifies HEAD,
//! index and working trees byte-exactly and runs no hooks. The source is a
//! registered repository (reused by origin, never duplicated) and each
//! shared checkout becomes a repo-local managed checkout of it, attached to
//! the joiner's thread as its real working checkout.
//!
//! Newer snapshots are never applied on their own: `ApplyShareUpdate`
//! moves attached checkouts forward, and refuses when one changed locally
//! since its restore. A deleted checkout folder is recreated in place.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use trama_db::CheckoutLocation;
use trama_domain::checkout_state::HeadRef;
use trama_domain::ids::{CheckoutId, RepositoryId, ThreadId};
use trama_domain::manifest::{ContentManifestV1, ManifestCheckout, ManifestRepository};
use trama_domain::share::ShareId;
use trama_domain::thread::{AttachMode, ThreadRepositoryAttachment};
use trama_domain::workspace::{Repository, WorkspaceMembership};
use trama_infra::git::share_git::{ExpectedState, ShareGitError, restore, share_refs};
use trama_infra::git::share_staging::{
    LocalState, materialize_share_checkout, prepare_staging, refresh_staging, update_share_checkout,
};
use trama_protocol::RestoredCheckout;

use super::super::CollabService;
use super::super::transport::ShareClient;
use super::snapshot::VerifiedSnapshot;
use crate::error::{DaemonError, Result};

/// What this device attached for one `(thread, repository)` of a share.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AttachedCheckout {
    pub checkout: RestoredCheckout,
    pub checkout_id: String,
    pub source: PathBuf,
    pub generation: u64,
    pub head: String,
    pub index_tree: String,
    pub working_tree: String,
}

impl AttachedCheckout {
    fn state(&self) -> LocalState {
        LocalState {
            head: self.head.clone(),
            index_tree: self.index_tree.clone(),
            working_tree: self.working_tree.clone(),
        }
    }

    fn matches(&self, co: &ManifestCheckout) -> bool {
        self.head == co.head_commit
            && self.index_tree == co.index_tree
            && self.working_tree == co.working_tree
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Attach what is missing; leave attached checkouts as they are.
    Restore,
    /// Also move attached checkouts to the manifest state.
    Update,
}

/// Short, stable source directory name: managed checkouts nest under it
/// (`.trama/worktrees/<thread>/<checkout>`), so long ids would overflow
/// Windows' 260-character path limit.
fn source_name(share_id: &str, repo_gid: &str) -> String {
    let key = format!("{share_id}\n{repo_gid}");
    let digest = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    digest.as_ref()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn checkout_key(share_id: &str, thread_gid: &str, repo_gid: &str) -> String {
    format!("share_checkout:{share_id}:{thread_gid}:{repo_gid}")
}

fn collab(e: impl std::fmt::Display) -> DaemonError {
    DaemonError::Collab(e.to_string())
}

fn expected_state(
    share_id: &str,
    repo: &ManifestRepository,
    co: &ManifestCheckout,
) -> Result<ExpectedState> {
    let (index_ref, working_ref) =
        share_refs(share_id, &format!("v{}", co.generation)).map_err(collab)?;
    Ok(ExpectedState {
        object_format: repo.git_object_format,
        head_commit: co.head_commit.clone(),
        head_ref: co.head.clone(),
        index_tree: co.index_tree.clone(),
        working_tree: co.working_tree.clone(),
        index_ref,
        working_ref,
    })
}

/// Brings `source` to `expected` from `bundle`: refreshed when it exists,
/// created otherwise (and removed again when that fails).
fn prepare_source(
    source: &Path,
    origin: Option<&str>,
    bundle: &Path,
    expected: &ExpectedState,
) -> Result<()> {
    if source.join(".git").exists() {
        return refresh_staging(source, origin)
            .and_then(|()| restore(source, bundle, expected))
            .map_err(collab);
    }
    let result = prepare_staging(source, expected.object_format, origin)
        .and_then(|()| restore(source, bundle, expected))
        .map_err(collab);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(source);
    }
    result
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(collab)?
}

impl CollabService {
    /// Fetches, verifies and restores the share's checkouts, attaching the
    /// missing ones. `None` when the snapshot carries no code.
    pub(crate) async fn restore_share_content(
        &self,
        client: &ShareClient,
        share_id: &str,
        snapshot: &VerifiedSnapshot,
    ) -> Result<Option<Vec<RestoredCheckout>>> {
        self.sync_checkouts(client, share_id, snapshot, Mode::Restore)
            .await
    }

    /// Moves attached checkouts to `snapshot` (and attaches new ones).
    pub(crate) async fn update_share_content(
        &self,
        client: &ShareClient,
        share_id: &str,
        snapshot: &VerifiedSnapshot,
    ) -> Result<Option<Vec<RestoredCheckout>>> {
        self.sync_checkouts(client, share_id, snapshot, Mode::Update)
            .await
    }

    /// True when some attached checkout differs from `manifest`.
    pub(crate) async fn checkouts_behind(
        &self,
        share_id: &str,
        manifest: &ContentManifestV1,
    ) -> Result<bool> {
        for co in &manifest.checkouts {
            if let Some(record) = self.attached(share_id, co).await? {
                if !record.matches(co) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn attached(
        &self,
        share_id: &str,
        co: &ManifestCheckout,
    ) -> Result<Option<AttachedCheckout>> {
        let key = checkout_key(share_id, &co.thread_gid.0, &co.repo_gid.0);
        let body = self.db().collab_setting_or_insert(&key, "").await?;
        Ok(serde_json::from_str(&body).ok())
    }

    async fn sync_checkouts(
        &self,
        client: &ShareClient,
        share_id: &str,
        snapshot: &VerifiedSnapshot,
        mode: Mode,
    ) -> Result<Option<Vec<RestoredCheckout>>> {
        let Some((manifest, bundles)) =
            self.fetch_share_content(client, share_id, snapshot).await?
        else {
            return Ok(None);
        };
        let root = self
            .credentials_path()?
            .parent()
            .map(|p| p.join("share-checkouts"))
            .ok_or_else(|| DaemonError::Invalid("no app directory".into()))?;
        let threads = self.db().share_threads(&ShareId::from(share_id)).await?;
        // Refuse an update before touching anything when any checkout moved.
        if mode == Mode::Update {
            for co in &manifest.checkouts {
                let Some(record) = self.attached(share_id, co).await? else {
                    continue;
                };
                let path = PathBuf::from(&record.checkout.path);
                if record.matches(co) || !path.exists() {
                    continue;
                }
                let now = blocking(move || {
                    trama_infra::git::share_staging::local_state(&path).map_err(collab)
                })
                .await?;
                if now != record.state() {
                    return Err(DaemonError::Invalid(format!(
                        "{} has local changes since the shared state was restored; commit or discard them before updating",
                        record.checkout.path
                    )));
                }
            }
        }
        let mut out = Vec::new();
        for co in &manifest.checkouts {
            let (repo, bundle) = locate(&manifest, &bundles, co)?;
            let expected = expected_state(share_id, repo, co)?;
            let key = checkout_key(share_id, &co.thread_gid.0, &co.repo_gid.0);
            let record = self.attached(share_id, co).await?;
            let done = match record {
                Some(record) if Path::new(&record.checkout.path).exists() => {
                    if record.matches(co) || mode == Mode::Restore {
                        out.push(record.checkout);
                        continue;
                    }
                    let (src, path, origin, b, e, prev) = (
                        record.source.clone(),
                        PathBuf::from(&record.checkout.path),
                        repo.origin_hint.clone(),
                        bundle.clone(),
                        expected.clone(),
                        record.state(),
                    );
                    blocking(move || {
                        prepare_source(&src, origin.as_deref(), &b, &e)?;
                        update_share_checkout(&src, &path, &prev, &e).map_err(|err| match err {
                            ShareGitError::StagingDirty(p) => DaemonError::Invalid(format!(
                                "{} has local changes since the shared state was restored",
                                p.display()
                            )),
                            other => collab(other),
                        })
                    })
                    .await?;
                    AttachedCheckout {
                        generation: co.generation,
                        head: co.head_commit.clone(),
                        index_tree: co.index_tree.clone(),
                        working_tree: co.working_tree.clone(),
                        ..record
                    }
                }
                record => {
                    // Threads added after the join are not mapped here yet.
                    let Some(local) = threads
                        .iter()
                        .find(|r| r.thread_gid == co.thread_gid)
                        .and_then(|r| r.local_thread_id.clone())
                        .map(ThreadId::from)
                    else {
                        continue;
                    };
                    self.materialize(share_id, &root, &local, repo, co, bundle, &expected, record)
                        .await?
                }
            };
            let body = serde_json::to_string(&done).map_err(collab)?;
            self.db().set_collab_setting(&key, &body).await?;
            out.push(done.checkout);
        }
        Ok(Some(out))
    }

    /// Restores into the source and provisions the managed checkout; with
    /// a `previous` record (folder deleted) it is recreated in place and
    /// not attached again.
    #[allow(clippy::too_many_arguments)]
    async fn materialize(
        &self,
        share_id: &str,
        root: &Path,
        thread_id: &ThreadId,
        repo: &ManifestRepository,
        co: &ManifestCheckout,
        bundle: &Path,
        expected: &ExpectedState,
        previous: Option<AttachedCheckout>,
    ) -> Result<AttachedCheckout> {
        let (repository, source) = match &previous {
            Some(p) => {
                let id = p
                    .checkout
                    .repository_id
                    .clone()
                    .ok_or_else(|| collab("record without repository"))?;
                (id, p.source.clone())
            }
            None => self.source_for(share_id, root, repo, co).await?,
        };
        let checkout = previous
            .as_ref()
            .map(|p| CheckoutId::from(p.checkout_id.as_str()))
            .unwrap_or_else(CheckoutId::generate);
        let manager = self.sessions().checkout_manager().clone();
        let (src, origin, b, e, t, r, c) = (
            source.clone(),
            repo.origin_hint.clone(),
            bundle.to_path_buf(),
            expected.clone(),
            thread_id.clone(),
            repository.clone(),
            checkout.clone(),
        );
        let provisioned = blocking(move || {
            prepare_source(&src, origin.as_deref(), &b, &e)?;
            materialize_share_checkout(&manager, &src, &t, &r, &c, &e).map_err(collab)
        })
        .await?;
        if previous.is_none() {
            self.register_source(thread_id, &repository, repo).await?;
            self.db()
                .attach_thread_checkout(
                    thread_id,
                    ThreadRepositoryAttachment {
                        repository_id: repository.clone(),
                        base_revision: Some(provisioned.base_revision.clone()),
                        checkout_id: Some(checkout.clone()),
                        attach_mode: AttachMode::Isolated,
                    },
                    CheckoutLocation {
                        checkout_id: checkout.clone(),
                        repository_id: repository.clone(),
                        path: provisioned.path.clone(),
                    },
                )
                .await?;
        }
        Ok(AttachedCheckout {
            checkout: RestoredCheckout {
                thread_id: Some(thread_id.clone()),
                repository_id: Some(repository),
                path: provisioned.path.to_string_lossy().into_owned(),
            },
            checkout_id: checkout.0,
            source,
            generation: co.generation,
            head: co.head_commit.clone(),
            index_tree: co.index_tree.clone(),
            working_tree: co.working_tree.clone(),
        })
    }

    /// The source repository for `repo`: an existing share source with the
    /// same origin, else this share's own (registered on first use).
    async fn source_for(
        &self,
        share_id: &str,
        root: &Path,
        repo: &ManifestRepository,
        co: &ManifestCheckout,
    ) -> Result<(RepositoryId, PathBuf)> {
        let canonical_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if let Some(origin) = &repo.origin_hint {
            for existing in self.db().list_all_repositories().await? {
                let Some(path) = existing.local_path.clone() else {
                    continue;
                };
                let ours = path.starts_with(root) || path.starts_with(&canonical_root);
                if existing.canonical_remote.as_deref() == Some(origin.as_str())
                    && ours
                    && path.join(".git").exists()
                {
                    return Ok((existing.id, path));
                }
            }
        }
        let dir = root.join(source_name(share_id, &co.repo_gid.0));
        if let Some(existing) = self.db().get_repository_by_local_path(&dir).await? {
            return Ok((existing.id, dir));
        }
        let branch = match &co.head {
            HeadRef::Branch { name } => Some(name.clone()),
            HeadRef::Detached => None,
        };
        let repository = Repository::new(repo.origin_hint.clone(), Some(dir.clone()), branch);
        self.db().create_repository(&repository).await?;
        Ok((repository.id, dir))
    }

    /// Makes the source a (not default-enabled) repository of the thread's
    /// workspace.
    async fn register_source(
        &self,
        thread_id: &ThreadId,
        repository: &RepositoryId,
        repo: &ManifestRepository,
    ) -> Result<()> {
        let thread = self
            .db()
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        if let Some(workspace_id) = &thread.workspace_id {
            self.db()
                .upsert_membership(&WorkspaceMembership {
                    workspace_id: workspace_id.clone(),
                    repository_id: repository.clone(),
                    alias: Some(repo.alias.clone()),
                    default_enabled: false,
                })
                .await?;
        }
        Ok(())
    }
}

fn locate<'a>(
    manifest: &'a ContentManifestV1,
    bundles: &'a [PathBuf],
    co: &ManifestCheckout,
) -> Result<(&'a ManifestRepository, &'a PathBuf)> {
    let repo = manifest
        .repositories
        .iter()
        .find(|r| r.repo_gid == co.repo_gid)
        .ok_or_else(|| collab("checkout without repository"))?;
    let object = co
        .object_index_refs
        .first()
        .and_then(|id| {
            manifest
                .object_index
                .iter()
                .position(|o| &o.content_id == id)
        })
        .ok_or_else(|| collab("checkout without bundle"))?;
    let bundle = bundles
        .get(object)
        .ok_or_else(|| collab("bundle missing"))?;
    Ok((repo, bundle))
}
