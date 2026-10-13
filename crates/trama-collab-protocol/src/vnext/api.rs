//! `/v3/shares` HTTP bodies and stream frames (docs/plan/03 §4).
//!
//! Event bodies and manifests are opaque to the server: manifests travel as
//! base64 ciphertext of a serialized signed envelope, so the server stores
//! and serves them without parsing.

use serde::{Deserialize, Serialize};

use super::events::ShareEventEnvelope;

/// Most events in one publish batch.
pub const MAX_SHARE_EVENTS_PER_PUBLISH: usize = 64;
/// Largest base64 manifest part accepted.
pub const MAX_MANIFEST_CIPHERTEXT_BYTES: usize = 4 * 1024 * 1024;
/// Request body limit for `/v3` routes (manifest commits are the largest).
pub const V3_BODY_LIMIT: usize = 2 * MAX_MANIFEST_CIPHERTEXT_BYTES + 64 * 1024;
/// Page size cap for `GET /v3/shares/{id}/events`.
pub const MAX_SHARE_EVENTS_PAGE: u32 = 500;
/// Longest writer lease a client can ask for.
pub const MAX_LEASE_TTL_SECS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareRole {
    Owner,
    Writer,
    Reader,
}

/// `POST /v3/shares`: the caller becomes owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterShareRequest {
    pub share_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub share_id: String,
    pub owner: String,
    pub role: ShareRole,
    /// May fetch the content manifest.
    pub read_code: bool,
    /// 0 before the first commit.
    pub latest_manifest_version: u64,
    /// 0 before the first event.
    pub last_server_seq: u64,
}

/// `POST /v3/shares/{id}/members` (owner only). Upserts the grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareMemberRequest {
    pub username: String,
    pub role: ShareRole,
    #[serde(default)]
    pub read_code: bool,
}

/// `POST /v3/shares/{id}/events`. All-or-nothing: one bad event rejects the
/// batch; already stored events (same `event_id`) are reported, not
/// re-appended, so retries are safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishShareEventsRequest {
    pub events: Vec<ShareEventEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedShareEvent {
    pub event_id: String,
    pub server_seq: u64,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishShareEventsResponse {
    pub events: Vec<PublishedShareEvent>,
}

/// `GET /v3/shares/{id}/events?after=&limit=`, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareEventsPage {
    pub events: Vec<ShareEventEnvelope>,
    pub last_server_seq: u64,
}

/// `POST /v3/shares/{id}/snapshots/commit`: compare-and-swap publish of
/// manifest `version` (must be `expected_version + 1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitManifestRequest {
    pub expected_version: u64,
    pub version: u64,
    /// Base64 ciphertext of the signed history manifest envelope.
    pub history: String,
    /// Base64 ciphertext of the signed content manifest envelope.
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestPart {
    History,
    Content,
}

/// `GET /v3/shares/{id}/manifest/{version}?part=history|content`
/// (`version` 0 = latest). Content needs `read_code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestBlob {
    pub version: u64,
    pub part: ManifestPart,
    pub ciphertext: String,
}

/// `POST /v3/shares/{id}/leases`: exclusive writer lease on one thread for
/// one device. Renewing by the holder extends it; another device gets 409.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRequest {
    pub thread_gid: String,
    pub device_id: String,
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseInfo {
    pub thread_gid: String,
    pub holder: String,
    pub device_id: String,
    /// Unix seconds.
    pub expires_at: i64,
}

/// Server → client on `GET /v3/shares/{id}/stream?after=`. Backfill then
/// live, strictly increasing `server_seq`, never skipping or repeating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShareServerFrame {
    Hello {
        protocol_version: u32,
        last_server_seq: u64,
    },
    Event {
        event: ShareEventEnvelope,
    },
    ManifestCommitted {
        version: u64,
    },
    Pong,
}

/// Client → server on the share stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShareClientFrame {
    Ping,
}

/// Largest base64 MLS key package / Welcome accepted.
pub const MAX_MLS_BLOB_BYTES: usize = 256 * 1024;

/// `POST /v3/shares/{id}/key-packages`: a granted member's device asks to
/// join the share's MLS group. Replaces an earlier request of that device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyPackageRequest {
    pub device_id: String,
    /// Base64 MLS key package.
    pub key_package: String,
}

/// `GET /v3/shares/{id}/key-packages` (owner): devices without a Welcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingKeyPackage {
    pub username: String,
    pub device_id: String,
    pub key_package: String,
}

/// `POST /v3/shares/{id}/welcomes` (owner) and
/// `GET /v3/shares/{id}/welcomes/{device_id}` (that device's user).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WelcomeBlob {
    pub device_id: String,
    /// Base64 MLS Welcome.
    pub welcome: String,
    /// Group epoch the Welcome joins at.
    pub epoch: u64,
}

/// Reply to `POST /v3/shares/{id}/members/{username}/revoke` (owner):
/// the grant, device bindings, key packages and leases of that user are
/// gone; `devices` must now be removed from the MLS group by the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokedMember {
    pub username: String,
    pub devices: Vec<String>,
}
