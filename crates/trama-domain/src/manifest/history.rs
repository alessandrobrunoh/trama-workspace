//! `HistoryManifestV1`: the perimeter and conversation slice visible to any
//! member with `read_history`. It never carries repository origins,
//! checkouts, object index or code keys (plan 03 §4 "Separazione ACL").

use super::ManifestError;
use crate::share::{DeviceId, ShareId, ShareScope, SharedThreadKind, ThreadGid};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const HISTORY_PAYLOAD_TYPE: &str = "trama.history_manifest.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkOrigin {
    pub parent_thread_gid: ThreadGid,
    /// Opaque checkpoint id, never a local path or session id.
    pub checkpoint_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestThread {
    pub thread_gid: ThreadGid,
    /// `None` exactly at the share boundary (the roots).
    pub parent_gid: Option<ThreadGid>,
    pub kind: SharedThreadKind,
    pub title: String,
    /// Opaque reference to the encrypted message slice of this thread.
    pub message_slice: Option<String>,
    pub fork_origin: Option<ForkOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryManifestV1 {
    pub schema_version: u32,
    pub share_id: ShareId,
    pub snapshot_version: u64,
    pub scope: ShareScope,
    pub root_thread_gids: Vec<ThreadGid>,
    pub include_future_descendants: bool,
    pub created_at: String,
    pub publisher_device_id: DeviceId,
    /// Watermark after which live sync resumes.
    pub history_cursor: String,
    pub threads: Vec<ManifestThread>,
}

impl HistoryManifestV1 {
    /// Structural checks: unique gids, roots are exactly the parentless
    /// threads, every parent is inside the perimeter, scope is respected.
    pub fn validate(&self) -> Result<(), ManifestError> {
        super::check_schema(self.schema_version)?;
        if self.root_thread_gids.is_empty() {
            return Err(ManifestError::Invalid("no root thread".into()));
        }
        let mut gids = BTreeSet::new();
        for thread in &self.threads {
            if !gids.insert(&thread.thread_gid) {
                return Err(ManifestError::Invalid(format!(
                    "duplicate thread {}",
                    thread.thread_gid
                )));
            }
        }
        let roots: BTreeSet<_> = self.root_thread_gids.iter().collect();
        if roots.len() != self.root_thread_gids.len() {
            return Err(ManifestError::Invalid("duplicate root thread".into()));
        }
        if self.scope == ShareScope::ThreadOnly && (roots.len() != 1 || self.threads.len() != 1) {
            return Err(ManifestError::Invalid(
                "thread_only share must contain exactly one thread".into(),
            ));
        }
        for thread in &self.threads {
            match &thread.parent_gid {
                None if !roots.contains(&thread.thread_gid) => {
                    return Err(ManifestError::Invalid(format!(
                        "thread {} has no parent but is not a root",
                        thread.thread_gid
                    )));
                }
                Some(_) if roots.contains(&thread.thread_gid) => {
                    return Err(ManifestError::Invalid(format!(
                        "root {} must have a null parent at the boundary",
                        thread.thread_gid
                    )));
                }
                Some(parent) if !gids.contains(parent) => {
                    return Err(ManifestError::Invalid(format!(
                        "parent {parent} of {} is outside the perimeter",
                        thread.thread_gid
                    )));
                }
                _ => {}
            }
        }
        if let Some(root) = roots.iter().find(|r| !gids.contains(**r)) {
            return Err(ManifestError::Invalid(format!("root {root} not listed")));
        }
        Ok(())
    }

    pub fn contains(&self, gid: &ThreadGid) -> bool {
        self.threads.iter().any(|t| &t.thread_gid == gid)
    }
}
