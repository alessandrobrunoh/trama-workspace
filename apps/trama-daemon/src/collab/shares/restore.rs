//! Joiner restore of shared checkouts (docs/plan/04 §restore). Each
//! checkout in the verified content manifest is materialized in its own
//! staging repository under `<app dir>/share-checkouts/<16 hex>/`: a fresh
//! repository with the manifest's object format, filled from the
//! `origin_hint` with the user's own Git credentials when there is one
//! (the bundle then omits objects origin already has), otherwise from the
//! bundle alone. `share_git::restore` verifies HEAD, index and working
//! trees byte-exactly; no hooks run. The staging then becomes a registered
//! repository and the source of a repo-local managed checkout attached to
//! the joiner's thread (the thread's real working checkout).

use std::path::{Path, PathBuf};

use trama_db::CheckoutLocation;
use trama_domain::checkout_state::HeadRef;
use trama_domain::ids::{CheckoutId, ThreadId};
use trama_domain::manifest::{ContentManifestV1, ManifestCheckout, ManifestRepository};
use trama_domain::share::ShareId;
use trama_domain::thread::{AttachMode, ThreadRepositoryAttachment};
use trama_domain::workspace::{Repository, WorkspaceMembership};
use trama_infra::git::share_git::{ExpectedState, restore, share_refs};
use trama_infra::git::share_staging::{materialize_share_checkout, prepare_staging};
use trama_protocol::RestoredCheckout;

use super::super::CollabService;
use super::super::transport::ShareClient;
use super::snapshot::VerifiedSnapshot;
use crate::error::{DaemonError, Result};

/// Short, stable staging directory name: managed checkouts nest under it
/// (`.trama/worktrees/<thread>/<checkout>`), so long ids would overflow
/// Windows' 260-character path limit.
fn staging_name(share_id: &str, thread_gid: &str, repo_gid: &str, generation: u64) -> String {
    let key = format!("{share_id}\n{thread_gid}\n{repo_gid}\n{generation}");
    let digest = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    digest.as_ref()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn checkout_key(share_id: &str, thread_gid: &str, repo_gid: &str) -> String {
    format!("share_checkout:{share_id}:{thread_gid}:{repo_gid}")
}

fn restore_one(
    dir: &Path,
    share_id: &str,
    repo: &ManifestRepository,
    co: &ManifestCheckout,
    bundle: &Path,
) -> Result<()> {
    let (index_ref, working_ref) =
        share_refs(share_id, &format!("v{}", co.generation)).map_err(collab)?;
    let expected = expected_state(repo, co, index_ref, working_ref);
    let result = prepare_staging(dir, repo.git_object_format, repo.origin_hint.as_deref())
        .and_then(|()| restore(dir, bundle, &expected))
        .map_err(collab);
    if result.is_err() {
        // A half-made staging is never offered as a checkout.
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

fn collab(e: impl std::fmt::Display) -> DaemonError {
    DaemonError::Collab(e.to_string())
}

impl CollabService {
    /// Fetches, verifies and restores the share's checkouts, attaching each
    /// one to its local thread as a managed checkout. `None` when the
    /// snapshot carries no code. Already attached checkouts are kept.
    pub(crate) async fn restore_share_content(
        &self,
        client: &ShareClient,
        share_id: &str,
        snapshot: &VerifiedSnapshot,
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
        let mut restored = Vec::new();
        for co in &manifest.checkouts {
            let (repo, bundle) = locate(&manifest, &bundles, co)?;
            let local = threads
                .iter()
                .find(|r| r.thread_gid == co.thread_gid)
                .and_then(|r| r.local_thread_id.clone())
                .map(ThreadId::from)
                .ok_or_else(|| collab("checkout for an unmapped thread"))?;
            let dir = root.join(staging_name(
                share_id,
                &co.thread_gid.0,
                &co.repo_gid.0,
                co.generation,
            ));
            let key = checkout_key(share_id, &co.thread_gid.0, &co.repo_gid.0);
            let attached = self.db().collab_setting_or_insert(&key, "").await?;
            if let Some(done) = serde_json::from_str::<RestoredCheckout>(&attached)
                .ok()
                .filter(|d| Path::new(&d.path).exists())
            {
                restored.push(done);
                continue;
            }
            if !dir.join(".git").exists() {
                let (d, s, r, c, b) = (
                    dir.clone(),
                    share_id.to_owned(),
                    repo.clone(),
                    co.clone(),
                    bundle.clone(),
                );
                tokio::task::spawn_blocking(move || restore_one(&d, &s, &r, &c, &b))
                    .await
                    .map_err(collab)??;
            }
            let done = self
                .attach_restored(share_id, &local, &dir, repo, co)
                .await?;
            let body = serde_json::to_string(&done).map_err(collab)?;
            self.db().set_collab_setting(&key, &body).await?;
            restored.push(done);
        }
        Ok(Some(restored))
    }

    /// The staging becomes a registered repository of the thread's
    /// workspace (not enabled by default) and the source of a repo-local
    /// managed checkout attached to the thread.
    async fn attach_restored(
        &self,
        share_id: &str,
        thread_id: &ThreadId,
        staging: &Path,
        repo: &ManifestRepository,
        co: &ManifestCheckout,
    ) -> Result<RestoredCheckout> {
        let thread = self
            .db()
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let branch = match &co.head {
            HeadRef::Branch { name } => Some(name.clone()),
            HeadRef::Detached => None,
        };
        let repository = Repository::new(
            repo.origin_hint.clone(),
            Some(staging.to_path_buf()),
            branch,
        );
        self.db().create_repository(&repository).await?;
        if let Some(workspace_id) = &thread.workspace_id {
            self.db()
                .add_membership(&WorkspaceMembership {
                    workspace_id: workspace_id.clone(),
                    repository_id: repository.id.clone(),
                    alias: Some(repo.alias.clone()),
                    default_enabled: false,
                })
                .await?;
        }
        let (index_ref, working_ref) =
            share_refs(share_id, &format!("v{}", co.generation)).map_err(collab)?;
        let expected = expected_state(repo, co, index_ref, working_ref);
        let checkout = CheckoutId::generate();
        let manager = self.sessions().checkout_manager().clone();
        let (src, t, r, c) = (
            staging.to_path_buf(),
            thread_id.clone(),
            repository.id.clone(),
            checkout.clone(),
        );
        let provisioned = tokio::task::spawn_blocking(move || {
            materialize_share_checkout(&manager, &src, &t, &r, &c, &expected)
        })
        .await
        .map_err(collab)?
        .map_err(collab)?;
        self.db()
            .attach_thread_checkout(
                thread_id,
                ThreadRepositoryAttachment {
                    repository_id: repository.id.clone(),
                    base_revision: Some(provisioned.base_revision.clone()),
                    checkout_id: Some(checkout.clone()),
                    attach_mode: AttachMode::Isolated,
                },
                CheckoutLocation {
                    checkout_id: checkout,
                    repository_id: repository.id.clone(),
                    path: provisioned.path.clone(),
                },
            )
            .await?;
        Ok(RestoredCheckout {
            thread_id: Some(thread_id.clone()),
            repository_id: Some(repository.id),
            path: provisioned.path.to_string_lossy().into_owned(),
        })
    }
}

fn expected_state(
    repo: &ManifestRepository,
    co: &ManifestCheckout,
    index_ref: String,
    working_ref: String,
) -> ExpectedState {
    ExpectedState {
        object_format: repo.git_object_format,
        head_commit: co.head_commit.clone(),
        head_ref: co.head.clone(),
        index_tree: co.index_tree.clone(),
        working_tree: co.working_tree.clone(),
        index_ref,
        working_ref,
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
