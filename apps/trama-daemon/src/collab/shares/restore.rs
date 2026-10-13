//! Joiner restore of shared checkouts (docs/plan/04 §restore). Each
//! checkout in the verified content manifest is materialized in its own
//! staging repository under `<app dir>/share-checkouts/<share>/`: a fresh
//! repository with the manifest's object format, filled from the
//! `origin_hint` with the user's own Git credentials when there is one
//! (the bundle then omits objects origin already has), otherwise from the
//! bundle alone. `share_git::restore` verifies HEAD, index and working
//! trees byte-exactly; no hooks run.

use std::path::{Path, PathBuf};

use trama_domain::manifest::{ContentManifestV1, ManifestCheckout, ManifestRepository};
use trama_infra::git::share_git::{ExpectedState, restore, share_refs};
use trama_infra::git::share_staging::prepare_staging;

use super::super::CollabService;
use super::super::transport::ShareClient;
use super::snapshot::VerifiedSnapshot;
use crate::error::{DaemonError, Result};

/// One path component from an id: never a separator or `..`.
fn component(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
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
    let expected = ExpectedState {
        object_format: repo.git_object_format,
        head_commit: co.head_commit.clone(),
        head_ref: co.head.clone(),
        index_tree: co.index_tree.clone(),
        working_tree: co.working_tree.clone(),
        index_ref,
        working_ref,
    };
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
    /// Fetches, verifies and restores the share's checkouts; returns the
    /// restored directories. Already restored generations are kept.
    pub(crate) async fn restore_share_content(
        &self,
        client: &ShareClient,
        share_id: &str,
        snapshot: &VerifiedSnapshot,
    ) -> Result<Vec<PathBuf>> {
        let Some((manifest, bundles)) =
            self.fetch_share_content(client, share_id, snapshot).await?
        else {
            return Ok(Vec::new());
        };
        let root = self
            .credentials_path()?
            .parent()
            .map(|p| p.join("share-checkouts").join(component(share_id)))
            .ok_or_else(|| DaemonError::Invalid("no app directory".into()))?;
        let mut restored = Vec::new();
        for co in &manifest.checkouts {
            let (repo, bundle) = locate(&manifest, &bundles, co)?;
            let dir = root.join(component(&co.thread_gid.0)).join(format!(
                "{}-v{}",
                component(&co.repo_gid.0),
                co.generation
            ));
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
            let key = checkout_key(share_id, &co.thread_gid.0, &co.repo_gid.0);
            self.db()
                .set_collab_setting(&key, &dir.to_string_lossy())
                .await?;
            restored.push(dir);
        }
        Ok(restored)
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
