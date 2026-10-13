//! `ContentManifestV1`: repositories, checkout states and encrypted object
//! index. Published as a separate envelope, only delivered to `read_code`
//! holders; it is bound to one history manifest by digest.

use super::ManifestError;
use crate::checkout_state::{CheckoutMode, HeadRef, ObjectFormat};
use crate::share::{DeviceId, RepoGid, ShareId, ThreadGid};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const CONTENT_PAYLOAD_TYPE: &str = "trama.content_manifest.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LfsPolicy {
    NotUsed,
    FetchFromOriginIfRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmodulePolicy {
    NotUsed,
    ExplicitConsentRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestRepository {
    pub repo_gid: RepoGid,
    pub alias: String,
    /// Suggested remote; a hint, never an authorization or identity.
    pub origin_hint: Option<String>,
    pub origin_identity_hint: Option<String>,
    pub git_object_format: ObjectFormat,
    pub requires_git_clone_permission: bool,
    pub lfs_policy: LfsPolicy,
    pub submodule_policy: SubmodulePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCheckout {
    pub thread_gid: ThreadGid,
    pub repo_gid: RepoGid,
    pub checkout_mode: CheckoutMode,
    pub generation: u64,
    pub base_commit: Option<String>,
    pub head_commit: String,
    pub index_tree: String,
    pub working_tree: String,
    pub head: HeadRef,
    pub fork_point: Option<String>,
    pub object_index_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestObject {
    /// Opaque id; never a plaintext hash.
    pub content_id: String,
    pub ciphertext_hash: String,
    pub byte_len: u64,
    pub key_epoch: u64,
    pub parts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentPolicySummary {
    pub ignored_files_exported: bool,
    pub untracked: String,
    /// Classes only, never the excluded paths themselves.
    pub excluded_path_classes: Vec<String>,
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentManifestV1 {
    pub schema_version: u32,
    pub share_id: ShareId,
    pub snapshot_version: u64,
    /// SHA-256 (hex) of the history envelope's canonical payload bytes.
    pub history_manifest_digest: String,
    pub created_at: String,
    pub publisher_device_id: DeviceId,
    pub repositories: Vec<ManifestRepository>,
    pub checkouts: Vec<ManifestCheckout>,
    pub object_index: Vec<ManifestObject>,
    pub content_policy: ContentPolicySummary,
}

/// Rejects hints that would leak local machine paths.
fn is_local_path(hint: &str) -> bool {
    let b = hint.as_bytes();
    hint.starts_with('/')
        || hint.starts_with('\\')
        || hint.starts_with('~')
        || hint.to_ascii_lowercase().starts_with("file:")
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

impl ContentManifestV1 {
    /// Internal consistency: unique repos/objects/states, OIDs match the
    /// repository format, refs resolve, no local paths in origin hints.
    pub fn validate(&self) -> Result<(), ManifestError> {
        super::check_schema(self.schema_version)?;
        let invalid = |msg: String| Err(ManifestError::Invalid(msg));
        let mut repos = std::collections::BTreeMap::new();
        for repo in &self.repositories {
            if repo.origin_hint.as_deref().is_some_and(is_local_path) {
                return invalid(format!("origin hint of {} is a local path", repo.repo_gid));
            }
            if repos
                .insert(&repo.repo_gid, repo.git_object_format)
                .is_some()
            {
                return invalid(format!("duplicate repository {}", repo.repo_gid));
            }
        }
        let mut objects = BTreeSet::new();
        for object in &self.object_index {
            if object.parts.is_empty() {
                return invalid(format!("object {} has no parts", object.content_id));
            }
            if !objects.insert(&object.content_id) {
                return invalid(format!("duplicate object {}", object.content_id));
            }
        }
        let mut states = BTreeSet::new();
        for co in &self.checkouts {
            let Some(format) = repos.get(&co.repo_gid) else {
                return invalid(format!("checkout references unknown repo {}", co.repo_gid));
            };
            if !states.insert((&co.thread_gid, &co.repo_gid)) {
                return invalid(format!(
                    "duplicate checkout {}/{}",
                    co.thread_gid, co.repo_gid
                ));
            }
            let oids = [&co.head_commit, &co.index_tree, &co.working_tree]
                .into_iter()
                .chain(co.base_commit.iter())
                .chain(co.fork_point.iter());
            if let Some(bad) = oids.into_iter().find(|o| !format.is_valid_oid(o)) {
                return invalid(format!("invalid object id {bad:?}"));
            }
            if let Some(r) = co.object_index_refs.iter().find(|r| !objects.contains(r)) {
                return invalid(format!("checkout references unknown object {r}"));
            }
        }
        Ok(())
    }
}
