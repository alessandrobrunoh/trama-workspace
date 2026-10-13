//! Checkout content in snapshots (docs/plan/03 §4, 04). The owner re-runs
//! the share preflight (secret scan, conflicts) at every publish and
//! refuses to export when it is not clean; each planned checkout is
//! captured under `refs/trama/share/<share>/v<version>/…`, exported as a
//! Git bundle without objects reachable from remote-tracking refs, sealed
//! as an encrypted `content` object and listed in a signed
//! `ContentManifestV1`. A `read_code` joiner downloads and verifies every
//! object, keeps the bundles under the app directory and restores them
//! (`restore.rs`).

use std::path::{Path, PathBuf};

use chrono::Utc;
use trama_collab_protocol::vnext::api::{ManifestPart, ObjectClass};
use trama_collab_protocol::vnext::cipher::PayloadKind;
use trama_db::{ShareRow, ShareThreadRow};
use trama_domain::checkout_state::CheckoutMode;
use trama_domain::ids::MessageId;
use trama_domain::manifest::{
    ContentManifestV1, ContentPolicySummary, LfsPolicy, MANIFEST_SCHEMA_VERSION, ManifestCheckout,
    ManifestObject, ManifestRepository, SignedEnvelope, SubmodulePolicy, open_content,
    seal_content,
};
use trama_domain::share::{DeviceId, RepoGid};
use trama_infra::git::share_git::{capture, export_bundle};

use super::super::CollabService;
use super::super::transport::{ShareClient, open_manifest};
use super::objects::{MAX_OBJECT_BYTES, SealedObject, download, open_object, seal_object, upload};
use super::snapshot::VerifiedSnapshot;
use crate::error::{DaemonError, Result};

fn collab(e: impl std::fmt::Display) -> DaemonError {
    DaemonError::Collab(e.to_string())
}

fn repo_gid(repository: &str) -> RepoGid {
    let digest = ring::digest::digest(&ring::digest::SHA256, repository.as_bytes());
    let hex: String = digest.as_ref()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    RepoGid(format!("rg_{hex}"))
}

fn remote_hint(repository: &str) -> Option<String> {
    (repository.contains("://") || repository.starts_with("git@")).then(|| repository.to_owned())
}

/// Tips of remote-tracking refs: the joiner fetches those from origin.
fn remote_tips(worktree: &Path) -> Vec<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["for-each-ref", "--format=%(objectname)", "refs/remotes"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

struct Exported {
    captured: trama_infra::git::share_git::CapturedState,
    object: SealedObject,
}

fn export(worktree: PathBuf, share_id: String, state: String, thin: bool) -> Result<Exported> {
    let captured = capture(&worktree, &share_id, &state).map_err(collab)?;
    // Without an origin hint the joiner restores from the bundle alone.
    let tips = if thin {
        remote_tips(&worktree)
    } else {
        Vec::new()
    };
    let prereqs: Vec<&str> = tips.iter().map(String::as_str).collect();
    let path = std::env::temp_dir().join(format!("trama-{}.bundle", MessageId::generate()));
    let result = export_bundle(&worktree, &captured, &prereqs, &path)
        .map_err(collab)
        .and_then(|()| std::fs::read(&path).map_err(collab));
    let _ = std::fs::remove_file(&path);
    let object = seal_object(&result?)?;
    Ok(Exported { captured, object })
}

impl CollabService {
    /// Owner: exports the share's checkouts and signs the content manifest
    /// bound to `history`. `None` when the scope has no checkouts.
    pub(crate) async fn build_content(
        &self,
        client: &ShareClient,
        row: &ShareRow,
        rows: &[ShareThreadRow],
        history: &SignedEnvelope,
        epoch: u64,
    ) -> Result<Option<SignedEnvelope>> {
        let share_id = row.share_id.0.clone();
        let Some(root) = rows
            .iter()
            .find(|r| r.source_scope_root)
            .and_then(|r| r.local_thread_id.clone())
        else {
            return Ok(None);
        };
        let plan = self.share_plan(&root.into(), row.scope).await?;
        if !plan.report.ok {
            return Err(DaemonError::Invalid(
                "content export blocked: the share preflight is not clean".into(),
            ));
        }
        let version = row.current_snapshot_version + 1;
        let (mut repositories, mut checkouts, mut objects) = (Vec::new(), Vec::new(), Vec::new());
        for co in plan.checkouts {
            let local = co.thread_id.to_string();
            let Some(gid) = rows
                .iter()
                .find(|r| r.local_thread_id.as_deref() == Some(local.as_str()))
            else {
                continue;
            };
            let (path, id, state) = (co.path.clone(), share_id.clone(), format!("v{version}"));
            let thin = remote_hint(&co.repository).is_some();
            let Exported { captured, object } =
                tokio::task::spawn_blocking(move || export(path, id, state, thin))
                    .await
                    .map_err(collab)??;
            upload(client, &share_id, ObjectClass::Content, &object).await?;
            let rg = repo_gid(&co.repository);
            if !repositories
                .iter()
                .any(|r: &ManifestRepository| r.repo_gid == rg)
            {
                repositories.push(ManifestRepository {
                    repo_gid: rg.clone(),
                    alias: co.repository.clone(),
                    origin_hint: remote_hint(&co.repository),
                    origin_identity_hint: None,
                    git_object_format: captured.object_format,
                    requires_git_clone_permission: true,
                    lfs_policy: LfsPolicy::NotUsed,
                    submodule_policy: SubmodulePolicy::NotUsed,
                });
            }
            checkouts.push(ManifestCheckout {
                thread_gid: gid.thread_gid.clone(),
                repo_gid: rg,
                checkout_mode: CheckoutMode::Owned,
                generation: version,
                base_commit: None,
                head_commit: captured.head_commit,
                index_tree: captured.index_tree,
                working_tree: captured.working_tree,
                head: captured.head_ref,
                fork_point: None,
                object_index_refs: vec![object.content_id.clone()],
            });
            objects.push(ManifestObject {
                parts: (0..object.parts.len()).map(|i| i.to_string()).collect(),
                content_id: object.content_id,
                ciphertext_hash: object.ciphertext_hash,
                byte_len: object.byte_len,
                key_epoch: epoch,
                object_key: Some(object.key),
            });
        }
        if checkouts.is_empty() {
            return Ok(None);
        }
        let e2ee = self.e2ee()?;
        let manifest = ContentManifestV1 {
            schema_version: MANIFEST_SCHEMA_VERSION,
            share_id: row.share_id.clone(),
            snapshot_version: version,
            history_manifest_digest: String::new(),
            created_at: Utc::now().to_rfc3339(),
            publisher_device_id: DeviceId(e2ee.device_id().to_owned()),
            repositories,
            checkouts,
            object_index: objects,
            content_policy: ContentPolicySummary {
                ignored_files_exported: false,
                untracked: "captured".into(),
                excluded_path_classes: Vec::new(),
                max_bytes: Some(MAX_OBJECT_BYTES as u64),
            },
        };
        let history = history.clone();
        let envelope = tokio::task::spawn_blocking(move || {
            let (key_id, key) = e2ee.signing_key()?;
            seal_content(manifest, &history, &key_id, &key)
                .map_err(|e| DaemonError::Invalid(e.to_string()))
        })
        .await
        .map_err(collab)??;
        Ok(Some(envelope))
    }

    /// `read_code` joiner: verifies the content manifest of `snapshot` and
    /// stores every bundle (in `object_index` order). `None` without content.
    pub(crate) async fn fetch_share_content(
        &self,
        client: &ShareClient,
        share_id: &str,
        snapshot: &VerifiedSnapshot,
    ) -> Result<Option<(ContentManifestV1, Vec<PathBuf>)>> {
        let blob = match client
            .manifest(share_id, snapshot.version, ManifestPart::Content)
            .await
        {
            Ok(blob) => blob,
            Err(DaemonError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let (cipher, id, epoch) = (
            self.e2ee()?.cipher.clone(),
            share_id.to_owned(),
            snapshot.epoch,
        );
        let envelope = tokio::task::spawn_blocking(move || {
            open_manifest(
                cipher.as_ref(),
                &id,
                epoch,
                PayloadKind::ContentManifest,
                &blob.ciphertext,
            )
        })
        .await
        .map_err(collab)??;
        let manifest = open_content(
            &envelope,
            &snapshot.keyring,
            &snapshot.envelope,
            &snapshot.manifest,
        )
        .map_err(|e| DaemonError::Collab(format!("content manifest rejected: {e}")))?;
        let dir = self
            .credentials_path()?
            .parent()
            .map(|p| p.join("share-content").join(share_id))
            .ok_or_else(|| DaemonError::Invalid("no app directory".into()))?;
        std::fs::create_dir_all(&dir).map_err(collab)?;
        let mut stored = Vec::new();
        for object in &manifest.object_index {
            let key = object
                .object_key
                .clone()
                .ok_or_else(|| DaemonError::Collab("content object without key".into()))?;
            let parts = u32::try_from(object.parts.len()).map_err(collab)?;
            let blobs = download(client, share_id, &object.content_id, parts).await?;
            let (cid, hash) = (object.content_id.clone(), object.ciphertext_hash.clone());
            let plain = tokio::task::spawn_blocking(move || open_object(&cid, &key, &blobs, &hash))
                .await
                .map_err(collab)??;
            if plain.len() as u64 != object.byte_len {
                return Err(DaemonError::Collab("content object length mismatch".into()));
            }
            let path = dir.join(format!("{}.bundle", object.content_id));
            std::fs::write(&path, plain).map_err(collab)?;
            stored.push(path);
        }
        Ok(Some((manifest, stored)))
    }
}
