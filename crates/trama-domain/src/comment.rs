use crate::ids::{CommentId, MessageId, RepositoryId, ThreadId};
use crate::message::Actor;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A comment anchored to code, designed to survive edits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAnchor {
    pub repository_id: RepositoryId,
    pub path: PathBuf,
    /// `FileRevision` number the comment was created against.
    pub revision: u64,
    pub start_line: u32,
    pub end_line: u32,
    /// Hash of the anchored text — used for relocation.
    pub text_hash: Option<String>,
    /// Hash of surrounding context lines (fuzzy matching fallback).
    pub context_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorStatus {
    /// Anchor relocated to a new position.
    Resolved { line: u32 },
    /// Ambiguous relocation; needs human confirmation.
    Fuzzy,
    /// Target gone (file deleted / text rewritten). Stay visible but
    /// detached — never silently reattach to the wrong line.
    Orphaned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum CommentTarget {
    MessageRange {
        message_id: MessageId,
        start_byte: usize,
        end_byte: usize,
    },
    Code { anchor: CodeAnchor },
    DiffHunk {
        repository_id: RepositoryId,
        path: PathBuf,
        base_revision: String,
        hunk_header: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub thread_id: ThreadId,
    pub author: Actor,
    pub target: CommentTarget,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}
