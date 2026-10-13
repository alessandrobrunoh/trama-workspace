//! Logical checkout state of one `(thread, repository)` inside a share
//! (plan `docs/plan/04` §2 and `05` §3). Immutable value types: a new
//! capture is a new generation, never an in-place update.

use crate::share::{RepoGid, ShareId, ThreadGid};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Git object format of a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            _ => None,
        }
    }

    /// Hex length of an object id in this format.
    pub fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }

    /// Whether `oid` is a full, lower-case object id of this format.
    pub fn is_valid_oid(self, oid: &str) -> bool {
        oid.len() == self.hex_len()
            && oid
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
}

/// Whether a thread owns its checkout or aliases another thread's (scouts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CheckoutMode {
    Owned,
    /// Metadata only: the alias is never a second writer.
    Alias {
        owner: ThreadGid,
    },
}

/// Where `HEAD` pointed at capture time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HeadRef {
    /// Short branch name (`main`), never a remote-tracking ref.
    Branch {
        name: String,
    },
    Detached,
}

/// Immutable id of one checkout capture: `thread + repo + generation`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StateId {
    pub thread_gid: ThreadGid,
    pub repo_gid: RepoGid,
    pub generation: u64,
}

impl fmt::Display for StateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}",
            self.thread_gid, self.repo_gid, self.generation
        )
    }
}

/// Summary of paths left out by content policy. Counts only: path names of
/// excluded files are never exported to unauthorized readers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OmittedPathsSummary {
    pub ignored: u64,
    pub denied_by_policy: u64,
}

/// The exact shareable state of a managed checkout: `HEAD`, the staged tree
/// of the real index and the working tree (plus admitted untracked files).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareCheckoutState {
    pub share_id: ShareId,
    pub state: StateId,
    pub previous_generation: Option<u64>,
    pub checkout_mode: CheckoutMode,
    pub object_format: ObjectFormat,
    pub head_commit_oid: String,
    pub head_ref: HeadRef,
    pub base_commit_oid: Option<String>,
    pub index_tree_oid: String,
    pub working_tree_oid: String,
    pub fork_point_oid: Option<String>,
    pub content_policy_version: u32,
    pub omitted: OmittedPathsSummary,
}

impl ShareCheckoutState {
    /// Every object id must be well formed for the declared format.
    pub fn validate(&self) -> Result<(), String> {
        let fmt = self.object_format;
        let mut oids = vec![
            ("head_commit_oid", Some(&self.head_commit_oid)),
            ("index_tree_oid", Some(&self.index_tree_oid)),
            ("working_tree_oid", Some(&self.working_tree_oid)),
            ("base_commit_oid", self.base_commit_oid.as_ref()),
            ("fork_point_oid", self.fork_point_oid.as_ref()),
        ];
        for (name, oid) in oids.drain(..) {
            if let Some(oid) = oid
                && !fmt.is_valid_oid(oid)
            {
                return Err(format!("{name} is not a valid {fmt:?} object id"));
            }
        }
        if let HeadRef::Branch { name } = &self.head_ref
            && (name.is_empty() || name.starts_with("refs/") || name.contains(".."))
        {
            return Err("head_ref must be a short branch name".into());
        }
        match self.previous_generation {
            Some(prev) if prev >= self.state.generation => {
                Err("previous_generation must precede generation".into())
            }
            None if self.state.generation != 0 => {
                Err("only generation 0 may lack a previous generation".into())
            }
            _ => Ok(()),
        }
    }

    /// Whether there are staged or unstaged changes on top of `HEAD`.
    pub fn is_clean(&self, head_tree_oid: &str) -> bool {
        self.index_tree_oid == head_tree_oid && self.working_tree_oid == head_tree_oid
    }
}

/// Compare-and-swap rule for publishing a new generation (plan 05 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PublishCheck {
    Accept,
    /// The publisher based its work on a stale generation: never overwrite,
    /// surface a divergence (review / fork / pull).
    Conflict {
        current: Option<u64>,
    },
}

/// Accept `candidate` only if it was based on the currently published
/// generation (`None` = nothing published yet).
pub fn check_publish(current: Option<u64>, candidate: &ShareCheckoutState) -> PublishCheck {
    let valid = match (current, candidate.previous_generation) {
        (None, None) => candidate.state.generation == 0,
        (Some(cur), Some(prev)) => cur == prev && candidate.state.generation > cur,
        _ => false,
    };
    if valid {
        PublishCheck::Accept
    } else {
        PublishCheck::Conflict { current }
    }
}

/// Whether the replica's history metadata is in sync with the share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    InSync,
    Pending,
    Offline,
    Diverged,
}

/// Whether checkout content is present and verified. Full history with
/// missing content is never ready to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentStatus {
    /// Event received; objects not yet fetched/verified.
    ContentPending,
    Verifying,
    Ready,
    /// The participant lacks `read_code`: history only.
    NotAuthorized,
    IntegrityFailed,
    Unavailable,
}

impl ContentStatus {
    pub fn is_runnable(self) -> bool {
        self == Self::Ready
    }
}

/// Single-writer lease on publishing a checkout revision (plan 05 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterLeaseState {
    Unassigned,
    Held,
    TransferRequested,
    Expired,
    Released,
    Conflict,
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0123456789abcdef0123456789abcdef01234567";
    const B: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn state(generation: u64, previous: Option<u64>) -> ShareCheckoutState {
        ShareCheckoutState {
            share_id: "share".into(),
            state: StateId {
                thread_gid: "t".into(),
                repo_gid: "r".into(),
                generation,
            },
            previous_generation: previous,
            checkout_mode: CheckoutMode::Owned,
            object_format: ObjectFormat::Sha1,
            head_commit_oid: A.into(),
            head_ref: HeadRef::Branch {
                name: "main".into(),
            },
            base_commit_oid: None,
            index_tree_oid: B.into(),
            working_tree_oid: B.into(),
            fork_point_oid: None,
            content_policy_version: 1,
            omitted: OmittedPathsSummary::default(),
        }
    }

    #[test]
    fn oid_validation() {
        assert!(ObjectFormat::Sha1.is_valid_oid(A));
        assert!(!ObjectFormat::Sha256.is_valid_oid(A));
        assert!(!ObjectFormat::Sha1.is_valid_oid(&A.to_uppercase()));
        assert!(!ObjectFormat::Sha1.is_valid_oid("HEAD"));
        assert_eq!(ObjectFormat::parse("sha256\n"), Some(ObjectFormat::Sha256));
    }

    #[test]
    fn state_validation() {
        assert!(state(0, None).validate().is_ok());
        assert!(state(3, Some(2)).validate().is_ok());
        assert!(state(3, None).validate().is_err());
        assert!(state(2, Some(2)).validate().is_err());
        let mut bad = state(0, None);
        bad.index_tree_oid = "abc".into();
        assert!(bad.validate().is_err());
        let mut bad = state(0, None);
        bad.head_ref = HeadRef::Branch {
            name: "refs/heads/main".into(),
        };
        assert!(bad.validate().is_err());
        assert!(state(0, None).is_clean(B));
        assert!(!state(0, None).is_clean(A));
    }

    #[test]
    fn publish_is_compare_and_swap() {
        assert_eq!(check_publish(None, &state(0, None)), PublishCheck::Accept);
        assert_eq!(
            check_publish(Some(1), &state(2, Some(1))),
            PublishCheck::Accept
        );
        assert_eq!(
            check_publish(Some(2), &state(2, Some(1))),
            PublishCheck::Conflict { current: Some(2) }
        );
        assert_eq!(
            check_publish(Some(0), &state(0, None)),
            PublishCheck::Conflict { current: Some(0) }
        );
        assert!(!ContentStatus::ContentPending.is_runnable());
        assert!(ContentStatus::Ready.is_runnable());
    }

    #[test]
    fn serde_shape() {
        let json = serde_json::to_value(state(0, None)).unwrap();
        assert_eq!(json["checkout_mode"]["mode"], "owned");
        assert_eq!(json["head_ref"]["kind"], "branch");
        assert_eq!(json["object_format"], "sha1");
        let back: ShareCheckoutState = serde_json::from_value(json).unwrap();
        assert_eq!(back, state(0, None));
    }
}
