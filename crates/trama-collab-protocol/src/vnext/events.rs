//! Durable share events (docs/plan/03 §5).
//!
//! The kind and routing metadata are cleartext so a server can sequence,
//! authorize and replay events; the body is `cipher_payload`, opaque to the
//! server. Timestamps are informational: causality (`prev_author_event`,
//! `parents`) and `server_seq` drive ordering.

use serde::{Deserialize, Serialize};

/// Largest `cipher_payload` (base64 text) accepted in one event.
pub const MAX_CIPHER_PAYLOAD_BYTES: usize = 192 * 1024;
/// Most causal parents one event may name.
pub const MAX_EVENT_PARENTS: usize = 32;
/// Longest id (event, share, device, thread) accepted.
pub const MAX_ID_LEN: usize = 128;

/// vNext durable event kinds. Unknown tags are denied, like the v2 kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareEventKind {
    ShareCreated,
    ScopeExpanded,
    ThreadAdded,
    ThreadRemovedFromScope,
    HistoryAppended,
    CommentChanged,
    ReviewVerdictPosted,
    CheckoutSnapshotPrepared,
    CheckoutSnapshotCommitted,
    CheckoutSnapshotSuperseded,
    MembershipChanged,
    KeyEpochAdvanced,
    PeerAcknowledged,
    ConflictDetected,
    SharePaused,
    ShareClosed,
}

impl ShareEventKind {
    /// Kinds scoped to one thread must carry `thread_gid`.
    pub fn requires_thread(self) -> bool {
        matches!(
            self,
            Self::ThreadAdded
                | Self::ThreadRemovedFromScope
                | Self::HistoryAppended
                | Self::CommentChanged
                | Self::ReviewVerdictPosted
        )
    }

    /// Kinds only the share owner may author (checked by the server ACL;
    /// listed here so clients reject forged ones too).
    pub fn owner_only(self) -> bool {
        matches!(
            self,
            Self::ShareCreated
                | Self::ScopeExpanded
                | Self::ThreadRemovedFromScope
                | Self::MembershipChanged
                | Self::KeyEpochAdvanced
                | Self::SharePaused
                | Self::ShareClosed
        )
    }
}

/// One durable vNext event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareEventEnvelope {
    /// Immutable id chosen by the author (stable across transports).
    pub event_id: String,
    pub share_id: String,
    pub kind: ShareEventKind,
    #[serde(default)]
    pub thread_gid: Option<String>,
    pub author_device: String,
    /// Per-device counter starting at 1; increases by one per event.
    pub author_seq: u64,
    /// The author's previous event, `None` only when `author_seq == 1`.
    #[serde(default)]
    pub prev_author_event: Option<String>,
    #[serde(default)]
    pub parents: Vec<String>,
    /// Assigned by a sequencing server; absent on unsequenced transports.
    #[serde(default)]
    pub server_seq: Option<u64>,
    pub key_epoch: u64,
    /// Base64 ciphertext of the kind-specific body.
    pub cipher_payload: String,
    /// Base64 signature by `author_device` over the canonical event bytes
    /// (encoding defined with the signed manifest envelope).
    pub signature: String,
    /// Author clock, unix milliseconds. Not authoritative for ordering.
    pub created_at: i64,
    /// Server receive time, unix milliseconds, when sequenced.
    #[serde(default)]
    pub received_at: Option<i64>,
}

/// A structurally invalid event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventShapeError {
    BadId(&'static str),
    MissingThread,
    UnexpectedThread,
    BadAuthorChain,
    TooManyParents,
    SelfParent,
    PayloadTooLarge,
    EmptySignature,
}

impl std::fmt::Display for EventShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::BadId(what) => return write!(f, "invalid {what}"),
            Self::MissingThread => "this event kind needs thread_gid",
            Self::UnexpectedThread => "this event kind must not carry thread_gid",
            Self::BadAuthorChain => "author_seq and prev_author_event disagree",
            Self::TooManyParents => "too many parents",
            Self::SelfParent => "an event cannot be its own parent",
            Self::PayloadTooLarge => "cipher_payload too large",
            Self::EmptySignature => "missing signature",
        };
        f.write_str(text)
    }
}

impl std::error::Error for EventShapeError {}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

impl ShareEventEnvelope {
    /// Shape checks that need no keys or state. Signature verification and
    /// ACL are separate steps.
    pub fn validate_shape(&self) -> Result<(), EventShapeError> {
        for (id, what) in [
            (&self.event_id, "event_id"),
            (&self.share_id, "share_id"),
            (&self.author_device, "author_device"),
        ] {
            if !valid_id(id) {
                return Err(EventShapeError::BadId(what));
            }
        }
        match (&self.thread_gid, self.kind.requires_thread()) {
            (Some(gid), _) if !valid_id(gid) => return Err(EventShapeError::BadId("thread_gid")),
            (None, true) => return Err(EventShapeError::MissingThread),
            (Some(_), false) if self.kind == ShareEventKind::ShareCreated => {
                return Err(EventShapeError::UnexpectedThread);
            }
            _ => {}
        }
        match (self.author_seq, &self.prev_author_event) {
            (0, _) | (1, Some(_)) => return Err(EventShapeError::BadAuthorChain),
            (n, None) if n > 1 => return Err(EventShapeError::BadAuthorChain),
            (_, Some(prev)) if !valid_id(prev) || prev == &self.event_id => {
                return Err(EventShapeError::BadAuthorChain);
            }
            _ => {}
        }
        if self.parents.len() > MAX_EVENT_PARENTS {
            return Err(EventShapeError::TooManyParents);
        }
        for parent in &self.parents {
            if parent == &self.event_id {
                return Err(EventShapeError::SelfParent);
            }
            if !valid_id(parent) {
                return Err(EventShapeError::BadId("parent"));
            }
        }
        if self.cipher_payload.len() > MAX_CIPHER_PAYLOAD_BYTES {
            return Err(EventShapeError::PayloadTooLarge);
        }
        if self.signature.is_empty() {
            return Err(EventShapeError::EmptySignature);
        }
        Ok(())
    }
}
