use super::*;

/// Disk used by one managed checkout. `apparent_bytes` is what `du` reports
/// (allocated size, blind to sharing); `private_bytes` is what deleting the
/// checkout would free: on APFS the blocks not shared with any clone
/// (`ATTR_CMNEXT_PRIVATESIZE`). When the volume cannot report it,
/// `private_estimated` is true and `private_bytes` equals the allocated size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsageItem {
    pub thread_id: ThreadId,
    pub thread_title: String,
    pub repository_id: RepositoryId,
    pub repository_title: String,
    pub path: PathBuf,
    pub apparent_bytes: u64,
    pub private_bytes: u64,
    #[serde(default)]
    pub private_estimated: bool,
    /// Latest message of the thread (its creation when it has none).
    #[serde(default)]
    pub last_active: Option<DateTime<Utc>>,
    #[serde(default)]
    pub archived: bool,
}

/// One checkout considered for removal, and the work that exists only in it.
/// `changed_files` lists up to 50 paths with uncommitted or untracked
/// (non-ignored) changes; `changed_file_count` counts all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutRemoval {
    pub repository_id: RepositoryId,
    pub title: String,
    pub path: PathBuf,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub changed_file_count: u32,
    /// Commits reachable from the checkout's branches that the source
    /// repository does not have.
    #[serde(default)]
    pub commits_not_in_source: u32,
    /// Bytes freed (or that would be freed) by removing the checkout.
    #[serde(default)]
    pub freed_bytes: Option<u64>,
}

impl CheckoutRemoval {
    /// True when removing the checkout needs `force`.
    pub fn has_unsaved_work(&self) -> bool {
        self.changed_file_count > 0 || self.commits_not_in_source > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutSnapshot {
    pub repository_id: RepositoryId,
    pub title: String,
    pub source_path: PathBuf,
    pub checkout_path: Option<PathBuf>,
    pub status: CheckoutStatus,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStatus {
    Ready,
    NeedsPreparation,
    Missing,
    Invalid,
    /// Removed to free disk (archived, or `DeleteThreadCheckouts`); recreated
    /// on demand from the saved snapshot.
    Removed,
}

/// Minimal workspace view for IPC. Not the domain `Workspace` aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    pub title: String,
    pub repo_count: u32,
}

/// Minimal thread view for IPC. Not the domain `Thread` aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub id: ThreadId,
    pub title: String,
    pub repo_count: u32,
    /// Set for subthreads; the UI nests them under this thread.
    #[serde(default)]
    pub parent_thread_id: Option<ThreadId>,
    #[serde(default = "top_kind")]
    pub kind: ThreadKind,
    /// Latest verdict of a review subthread.
    #[serde(default)]
    pub review_verdict: Option<ReviewVerdict>,
    /// The thread's model override; `None` uses the workspace default.
    #[serde(default)]
    pub model: Option<String>,
    /// Number of ancestors (0 for a top-level thread). Nesting has no limit;
    /// `parent_thread_id` alone is enough to rebuild the tree.
    #[serde(default)]
    pub depth: u32,
    /// Set for threads spawned by another thread's agent (Trama subagents).
    #[serde(default)]
    pub subagent: Option<SubagentSnapshot>,
    /// Archived threads are hidden from the active list; their history is
    /// kept and their checkouts may have been removed.
    #[serde(default)]
    pub archived: bool,
}

pub(super) fn top_kind() -> ThreadKind {
    ThreadKind::Top
}

/// How a subagent thread was spawned and where its task is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    pub spawned_by: SpawnedBy,
    pub status: SubagentStatus,
    /// Failure reason, or what the last sync into the parent did.
    pub detail: Option<String>,
}

/// One workspace membership. `title` is the membership alias when set.
/// `path` is the canonical local working tree, not a remote URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    pub id: RepositoryId,
    pub title: String,
    pub path: PathBuf,
    pub default_enabled: bool,
}
