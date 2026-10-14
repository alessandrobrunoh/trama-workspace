//! Plaintext bodies of vNext events, encrypted into `cipher_payload`.
//! Only kinds whose local effect the plan defines have a body here.

use serde::{Deserialize, Serialize};

/// Most messages in one `history_appended` body.
pub const MAX_HISTORY_MESSAGES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryMessage {
    /// Author-stable message id, for idempotent local apply.
    pub origin: String,
    pub role: HistoryRole,
    pub text: String,
    /// RFC 3339; informational.
    pub created_at: String,
}

/// Body of `history_appended`: messages appended to `thread_gid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryAppendedBody {
    pub messages: Vec<HistoryMessage>,
}

/// Body of `review_verdict_posted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewVerdictBody {
    pub verdict: String,
    #[serde(default)]
    pub body: String,
}

/// Body of `thread_added`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadAddedBody {
    pub parent_gid: Option<String>,
    pub kind: String,
    pub title: String,
}
