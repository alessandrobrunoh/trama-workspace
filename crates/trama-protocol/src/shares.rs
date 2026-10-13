//! Share/Join flow payloads (protocol 14, docs/plan/02 §4–§6).
//!
//! Flow: `SharePreflight` → `CreateShare` (owner) → joiner
//! `RequestShareJoin` (publishes a key package) → owner
//! `ListShareJoinRequests` / `ApproveShareJoin` (adds the device to the MLS
//! group) → joiner `CompleteShareJoin` (Welcome, manifests, local threads).
//! Then `PublishShareSnapshot` and writer leases. Every reply can also be a
//! `DaemonMessage::Error`.

use super::*;
pub use trama_collab_protocol::vnext::api::ShareRole;
pub use trama_domain::secrets::{SecretFinding, SecretRule};
pub use trama_domain::share::{ShareScope, ShareStatus, SharedThreadKind};

/// One member to grant on create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInvite {
    pub username: String,
    /// `writer` or `reader`; `owner` is refused.
    pub role: ShareRole,
    #[serde(default)]
    pub read_code: bool,
}

/// Why a share cannot be created. Every blocker blocks; there is no
/// override in v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShareBlocker {
    /// Not signed in to a collab service.
    NotConnected,
    /// The service does not offer the needed vNext capabilities.
    ServerUnsupported { missing: Vec<String> },
    /// Unresolved merge conflicts in a checkout's index.
    GitConflicts {
        thread_id: ThreadId,
        repository: String,
        paths: Vec<String>,
    },
    /// Credentials found in files or Git objects to be exported. Findings
    /// never contain the secret.
    Secrets { findings: Vec<SecretFinding> },
    /// A scout aliases a checkout owned by a thread outside the share.
    ScoutOutsideShare { scout: ThreadId, owner: ThreadId },
    /// The perimeter could not be resolved (unknown root, cycle…).
    Perimeter { message: String },
    /// A checkout could not be inspected (missing, not a repository…).
    CheckoutUnavailable {
        thread_id: ThreadId,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePreflightThread {
    pub thread_id: ThreadId,
    pub title: String,
    pub kind: SharedThreadKind,
    /// `None` at the share boundary.
    pub parent_thread_id: Option<ThreadId>,
    pub is_scope_root: bool,
}

/// Reply to `SharePreflight` (and the gate of `CreateShare`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePreflightReport {
    pub thread_id: ThreadId,
    pub scope: ShareScope,
    /// True when `blockers` is empty.
    pub ok: bool,
    pub threads: Vec<SharePreflightThread>,
    /// Repository aliases whose checkouts would be exported.
    pub repositories: Vec<String>,
    /// Files and Git objects scanned for secrets.
    pub scanned_files: u32,
    pub blockers: Vec<ShareBlocker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSnapshot {
    pub share_id: String,
    pub scope: ShareScope,
    pub status: ShareStatus,
    pub role: ShareRole,
    pub server_url: Option<String>,
    /// Local thread of the share root, once mapped.
    pub root_thread_id: Option<ThreadId>,
    pub snapshot_version: u64,
    /// Current MLS epoch, when this device is in the group.
    pub key_epoch: Option<u64>,
}

/// A device waiting for the owner to admit it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareJoinRequest {
    pub username: String,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLeaseSnapshot {
    pub share_id: String,
    /// Local thread, when the leased thread is mapped on this device.
    pub thread_id: Option<ThreadId>,
    pub thread_gid: String,
    pub holder: String,
    pub device_id: String,
    /// Unix seconds.
    pub expires_at: i64,
}
