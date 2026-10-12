//! Versioned JSON-lines IPC between the desktop UI, the local daemon, and (later) runners.
//!
//! # Framing
//!
//! Each message is a single JSON object encoded as UTF-8 and terminated by a newline
//! (`\n`). There is no length prefix and no embedded raw newlines inside a value
//! (`serde_json::to_string` emits compact JSON). A transport is any reliable byte
//! stream (Unix domain socket today; TCP/SSH for remote runners later).
//!
//! Decode one message per line; ignore empty lines. A line that is not valid JSON
//! or does not match a known schema is a protocol error.
//!
//! # Tagging
//!
//! [`ClientMessage`] and [`DaemonMessage`] are **adjacently tagged** serde enums:
//! `{"type":"<snake_case_variant>","payload": ... }`. Unit variants omit `payload`.
//!
//! # Unknown-tag policy
//!
//! Unknown `type` tags are **denied**. Serde rejects them at deserialize time; the
//! daemon must not silently ignore a variant it does not understand. Because of that,
//! a new variant is a breaking handshake change: an older peer would accept `Hello`
//! and then fail the command. [`PROTOCOL_VERSION`] **2** added workspace creation,
//! repository membership, and legacy-thread migration. **3** added thread
//! checkout discovery. **4** added the approval channel
//! ([`ClientMessage::ApprovalHook`], [`DaemonMessage::ApprovalHookDecision`])
//! and the `session_info` / `approval_resolved` agent events. **5** adds
//! subthreads, changes, pull changes, review verdicts and land results. Each
//! bump is transport-only. Persisted domain ids and SQLite rows are not rewritten.
//!
//! A version mismatch fails the handshake (`Welcome.accepted = false`). Extra object
//! fields inside a known payload are ignored (serde default). Bump [`PROTOCOL_VERSION`]
//! again for further tag or payload shape changes.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::PathBuf;

pub use trama_domain::agent::{AgentEvent, ApprovalDecision, BillingSource, UsageReport};
pub use trama_domain::ids::{RepositoryId, ThreadId, WorkspaceId};
pub use trama_domain::message::Message;
pub use trama_domain::subthread::{
    FileChange, FileChangeStatus, LandOutcome, PullFileOutcome, PullFileResult, RepoPullResult,
    ReviewVerdict,
};
pub use trama_domain::thread::ThreadKind;

/// Wire protocol version carried in `Hello` / `Welcome`.
///
/// Version 5 adds subthreads (create/list, parent and kind in thread
/// snapshots), changes, pull changes, review verdicts and land results.
/// Version 4 added the Claude approval channel and 3 thread checkout
/// discovery. It does not migrate stored data; peers with a different version
/// are rejected at handshake time.
pub const PROTOCOL_VERSION: u32 = 5;

/// Environment variable that relocates the whole app-data directory (socket,
/// database, config, legacy checkouts). Use it for isolated test instances
/// instead of overriding `HOME`: the `claude` child still needs the real
/// `HOME` to find the user's Claude Code login.
pub const DATA_DIR_ENV: &str = "TRAMA_DATA_DIR";

/// `$TRAMA_DATA_DIR`, else `$HOME/Library/Application Support/TramaWorkspace`.
pub fn default_app_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/TramaWorkspace"))
}

/// Encode `value` as one JSON line (compact JSON plus a trailing `\n`).
pub fn encode_json_line<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    Ok(buf)
}

/// Decode a single JSON line. A trailing `\n` / `\r\n` is tolerated.
pub fn decode_json_line<T: DeserializeOwned>(line: &str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n']))
}

/// UI → daemon (and later runner).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        client_name: String,
    },
    ListWorkspaces,
    ListThreads {
        workspace_id: WorkspaceId,
    },
    CreateThread {
        workspace_id: WorkspaceId,
        title: String,
    },
    SubscribeThread {
        thread_id: ThreadId,
    },
    UnsubscribeThread {
        thread_id: ThreadId,
    },
    SendPrompt {
        thread_id: ThreadId,
        text: String,
    },
    Interrupt {
        thread_id: ThreadId,
    },
    RespondApproval {
        thread_id: ThreadId,
        request_id: String,
        decision: ApprovalDecision,
    },
    /// Create a workspace in the database. Does not edit `config.json`.
    CreateWorkspace {
        name: String,
    },
    /// Attach an existing local Git working tree. Not a remote clone.
    AddRepository {
        workspace_id: WorkspaceId,
        path: PathBuf,
        alias: String,
    },
    ListRepositories {
        workspace_id: WorkspaceId,
    },
    /// Explicit command. Prepares isolated clones and records checkout metadata.
    /// Does not run the agent and does not modify the source working trees.
    MigrateLegacyThread {
        thread_id: ThreadId,
    },
    GetThreadCheckouts {
        thread_id: ThreadId,
    },
    /// Sent only by the `trama-daemon approval-hook` helper that Claude Code
    /// runs as its `PermissionRequest` hook. `token` identifies the agent
    /// session (from the hook's environment). The daemon answers once with
    /// [`DaemonMessage::ApprovalHookDecision`]; closing the connection first
    /// cancels the request. UI clients never send this.
    ApprovalHook {
        token: String,
        tool_name: String,
        tool_input: serde_json::Value,
    },
    /// Fork a subthread from a top-level thread: copies of the parent's
    /// checkouts (uncommitted and untracked changes included) and, when the
    /// parent has an agent session, its conversation. Review and land
    /// subthreads start their first turn immediately; a plain subthread only
    /// when `initial_prompt` is set. Answered with `SubthreadCreated`.
    CreateSubthread {
        parent_thread_id: ThreadId,
        kind: SubthreadKind,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        initial_prompt: Option<String>,
    },
    /// Answered with `Subthreads`.
    ListSubthreads {
        parent_thread_id: ThreadId,
    },
    /// Unified diff per repository of the thread's current working state
    /// (untracked non-ignored files included) against `base`. Answered with
    /// `Changes`.
    GetChanges {
        thread_id: ThreadId,
        #[serde(default)]
        base: ChangeBase,
    },
    /// Apply what a subthread changed since its fork point to its parent's
    /// checkouts. Never commits. Answered with `ChangesPulled`.
    PullChanges {
        subthread_id: ThreadId,
    },
    /// Record a review subthread's verdict and post it to the parent thread.
    /// Answered with `ReviewVerdictPosted`.
    SubmitReviewVerdict {
        review_thread_id: ThreadId,
        verdict: ReviewVerdict,
    },
    Shutdown,
}

/// The kind of subthread to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubthreadKind {
    Plain,
    Review,
    Land,
}

impl SubthreadKind {
    pub fn thread_kind(self) -> ThreadKind {
        match self {
            Self::Plain => ThreadKind::Sub,
            Self::Review => ThreadKind::Review,
            Self::Land => ThreadKind::Land,
        }
    }
}

/// What `GetChanges` diffs against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeBase {
    /// Where the thread started: the base revision of a top-level thread's
    /// checkout, or a subthread's fork point (the parent's full working state
    /// when the subthread was created).
    #[default]
    ThreadBase,
    /// The checkout's last commit (`HEAD`): uncommitted changes only.
    Head,
    /// The working state when the latest agent turn started.
    LastTurn,
}

/// One repository's changes. `base_revision` is the resolved base commit.
/// `diff` is `git diff --no-color -M` text (binary contents are not inlined)
/// and is cut at 8 MiB when `truncated`; `files` is always complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoChanges {
    pub repository_id: RepositoryId,
    pub title: String,
    pub base_revision: String,
    pub diff: String,
    pub truncated: bool,
    pub files: Vec<FileChange>,
}

/// Daemon → UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum DaemonMessage {
    Welcome {
        protocol_version: u32,
        daemon_version: String,
        accepted: bool,
    },
    Workspaces(Vec<WorkspaceSnapshot>),
    Threads(Vec<ThreadSnapshot>),
    ThreadCreated(ThreadSnapshot),
    MessageAppended {
        thread_id: ThreadId,
        message: Message,
    },
    AgentEventEnvelope {
        thread_id: ThreadId,
        event: AgentEvent,
    },
    UsageSnapshot {
        thread_id: ThreadId,
        totals: UsageReport,
    },
    Error {
        code: String,
        message: String,
    },
    WorkspaceCreated(WorkspaceSnapshot),
    Repositories {
        workspace_id: WorkspaceId,
        repositories: Vec<RepositorySnapshot>,
    },
    RepositoryAdded {
        workspace_id: WorkspaceId,
        repository: RepositorySnapshot,
    },
    ThreadMigrated(ThreadSnapshot),
    /// Reply to [`ClientMessage::ApprovalHook`], sent only on that hook's
    /// connection. `message` explains a denial to the agent; `interrupt`
    /// asks the agent to stop the turn (set when the user interrupted).
    ApprovalHookDecision {
        decision: ApprovalDecision,
        message: Option<String>,
        interrupt: bool,
    },
    ThreadCheckouts {
        thread_id: ThreadId,
        checkouts: Vec<CheckoutSnapshot>,
    },
    /// Reply to `CreateSubthread`; also broadcast to the parent's subscribers.
    SubthreadCreated(ThreadSnapshot),
    Subthreads {
        parent_thread_id: ThreadId,
        threads: Vec<ThreadSnapshot>,
    },
    Changes {
        thread_id: ThreadId,
        base: ChangeBase,
        repos: Vec<RepoChanges>,
    },
    /// Per-file outcome of `PullChanges`; also broadcast to the parent's
    /// subscribers. Conflicts are `PullFileOutcome::Conflict` entries.
    ChangesPulled {
        subthread_id: ThreadId,
        parent_thread_id: ThreadId,
        repos: Vec<RepoPullResult>,
    },
    /// A review verdict was recorded. Sent to the requester and the
    /// subscribers of both threads; the parent also gets a persisted system
    /// message (replayed on subscribe).
    ReviewVerdictPosted {
        review_thread_id: ThreadId,
        parent_thread_id: ThreadId,
        verdict: ReviewVerdict,
    },
    /// A land subthread reported its result (broadcast to the subscribers of
    /// both threads; the parent also gets a persisted system message).
    LandResult {
        land_thread_id: ThreadId,
        parent_thread_id: ThreadId,
        outcome: LandOutcome,
        summary: String,
    },
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
}

fn top_kind() -> ThreadKind {
    ThreadKind::Top
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

#[cfg(test)]
mod tests {
    use super::*;
    use trama_domain::agent::AgentEvent;
    use trama_domain::ids::ThreadId;
    use trama_domain::message::Message;

    fn roundtrip_eq<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value);
        let again = serde_json::to_string(&back).unwrap();
        assert_eq!(again, json);
    }

    fn roundtrip_json<T>(value: &T)
    where
        T: Serialize + DeserializeOwned,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::to_value(&back).unwrap()
        );
        let line = encode_json_line(value).unwrap();
        assert_eq!(*line.last().unwrap(), b'\n');
        assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);
        let decoded: T = decode_json_line(std::str::from_utf8(&line).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::to_value(&decoded).unwrap()
        );
    }

    fn workspace_id() -> WorkspaceId {
        WorkspaceId::from("01hworkspace000000000000001")
    }

    fn thread_id() -> ThreadId {
        ThreadId::from("01hthread0000000000000000001")
    }

    fn snapshot_ws() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: workspace_id(),
            title: "LaFraternita".into(),
            repo_count: 2,
        }
    }

    fn snapshot_th() -> ThreadSnapshot {
        ThreadSnapshot {
            id: thread_id(),
            title: "Fix auth".into(),
            repo_count: 1,
            parent_thread_id: None,
            kind: ThreadKind::Top,
            review_verdict: None,
        }
    }

    fn snapshot_repo() -> RepositorySnapshot {
        RepositorySnapshot {
            id: RepositoryId::from("01hrepo000000000000000000001"),
            title: "app".into(),
            path: std::path::PathBuf::from("/src/app"),
            default_enabled: true,
        }
    }

    #[test]
    fn client_message_roundtrip_every_kind() {
        let tid = thread_id();
        let wid = workspace_id();
        let kinds = [
            ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "trama-desktop".into(),
            },
            ClientMessage::ListWorkspaces,
            ClientMessage::ListThreads {
                workspace_id: wid.clone(),
            },
            ClientMessage::CreateThread {
                workspace_id: wid.clone(),
                title: "New thread".into(),
            },
            ClientMessage::SubscribeThread {
                thread_id: tid.clone(),
            },
            ClientMessage::UnsubscribeThread {
                thread_id: tid.clone(),
            },
            ClientMessage::SendPrompt {
                thread_id: tid.clone(),
                text: "hello".into(),
            },
            ClientMessage::Interrupt {
                thread_id: tid.clone(),
            },
            ClientMessage::RespondApproval {
                thread_id: tid.clone(),
                request_id: "req-1".into(),
                decision: ApprovalDecision::AllowOnce,
            },
            ClientMessage::CreateWorkspace {
                name: "LaFraternita".into(),
            },
            ClientMessage::AddRepository {
                workspace_id: wid,
                path: std::path::PathBuf::from("/src/app"),
                alias: "app".into(),
            },
            ClientMessage::ListRepositories {
                workspace_id: workspace_id(),
            },
            ClientMessage::MigrateLegacyThread { thread_id: tid },
            ClientMessage::ApprovalHook {
                token: "tok".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
            },
            ClientMessage::Shutdown,
        ];
        for msg in &kinds {
            roundtrip_eq(msg);
            roundtrip_json(msg);
        }
        assert_eq!(kinds.len(), 15);
    }

    #[test]
    fn daemon_message_roundtrip_every_kind() {
        let tid = thread_id();
        let message = Message::new_user(tid.clone(), "alice", "hi");
        let kinds = [
            DaemonMessage::Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".into(),
                accepted: true,
            },
            DaemonMessage::Workspaces(vec![snapshot_ws()]),
            DaemonMessage::Threads(vec![snapshot_th()]),
            DaemonMessage::ThreadCreated(snapshot_th()),
            DaemonMessage::MessageAppended {
                thread_id: tid.clone(),
                message,
            },
            DaemonMessage::AgentEventEnvelope {
                thread_id: tid.clone(),
                event: AgentEvent::TextDelta {
                    text: "chunk".into(),
                },
            },
            DaemonMessage::UsageSnapshot {
                thread_id: tid,
                totals: UsageReport {
                    model: "mock".into(),
                    input_tokens: 3,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 1,
                },
            },
            DaemonMessage::Error {
                code: "not_found".into(),
                message: "thread missing".into(),
            },
            DaemonMessage::WorkspaceCreated(snapshot_ws()),
            DaemonMessage::Repositories {
                workspace_id: workspace_id(),
                repositories: vec![snapshot_repo()],
            },
            DaemonMessage::RepositoryAdded {
                workspace_id: workspace_id(),
                repository: snapshot_repo(),
            },
            DaemonMessage::ThreadMigrated(snapshot_th()),
            DaemonMessage::ApprovalHookDecision {
                decision: ApprovalDecision::Deny,
                message: Some("denied".into()),
                interrupt: false,
            },
        ];
        for msg in &kinds {
            roundtrip_json(msg);
        }
        assert_eq!(kinds.len(), 13);
    }

    #[test]
    fn unknown_tags_are_denied() {
        let client =
            serde_json::from_str::<ClientMessage>(r#"{"type":"launch_nukes","payload":{}}"#);
        assert!(client.is_err(), "unknown client tags must be denied");
        let daemon =
            serde_json::from_str::<DaemonMessage>(r#"{"type":"secret_backdoor","payload":null}"#);
        assert!(daemon.is_err(), "unknown daemon tags must be denied");
        let client_err = client.unwrap_err().to_string();
        assert!(
            client_err.contains("unknown variant") || client_err.contains("launch_nukes"),
            "{client_err}"
        );
    }

    #[test]
    fn hello_json_shape_is_adjacently_tagged() {
        let json = serde_json::to_string(&ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "ui".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"type":"hello","payload":{{"protocol_version":{PROTOCOL_VERSION},"client_name":"ui"}}}}"#
            )
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::ListWorkspaces).unwrap(),
            r#"{"type":"list_workspaces"}"#
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::Workspaces(vec![])).unwrap(),
            r#"{"type":"workspaces","payload":[]}"#
        );
    }

    #[test]
    fn onboarding_and_migration_tags_keep_existing_shape() {
        let workspace = workspace_id();
        let repo = snapshot_repo();
        assert_eq!(
            serde_json::to_string(&ClientMessage::CreateWorkspace {
                name: "Demo".into(),
            })
            .unwrap(),
            r#"{"type":"create_workspace","payload":{"name":"Demo"}}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::AddRepository {
                workspace_id: workspace.clone(),
                path: std::path::PathBuf::from("/src/app"),
                alias: "app".into(),
            })
            .unwrap(),
            format!(
                r#"{{"type":"add_repository","payload":{{"workspace_id":"{workspace}","path":"/src/app","alias":"app"}}}}"#
            )
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::ListRepositories {
                workspace_id: workspace.clone(),
            })
            .unwrap(),
            format!(r#"{{"type":"list_repositories","payload":{{"workspace_id":"{workspace}"}}}}"#)
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::MigrateLegacyThread {
                thread_id: thread_id(),
            })
            .unwrap(),
            format!(
                r#"{{"type":"migrate_legacy_thread","payload":{{"thread_id":"{}"}}}}"#,
                thread_id()
            )
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::WorkspaceCreated(snapshot_ws())).unwrap(),
            format!(
                r#"{{"type":"workspace_created","payload":{{"id":"{workspace}","title":"LaFraternita","repo_count":2}}}}"#
            )
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::Repositories {
                workspace_id: workspace.clone(),
                repositories: vec![repo.clone()],
            })
            .unwrap(),
            format!(
                r#"{{"type":"repositories","payload":{{"workspace_id":"{workspace}","repositories":[{{"id":"{}","title":"app","path":"/src/app","default_enabled":true}}]}}}}"#,
                repo.id
            )
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::RepositoryAdded {
                workspace_id: workspace.clone(),
                repository: repo.clone(),
            })
            .unwrap(),
            format!(
                r#"{{"type":"repository_added","payload":{{"workspace_id":"{workspace}","repository":{{"id":"{}","title":"app","path":"/src/app","default_enabled":true}}}}}}"#,
                repo.id
            )
        );
        let thread = thread_id();
        assert_eq!(
            serde_json::to_string(&DaemonMessage::ThreadMigrated(snapshot_th())).unwrap(),
            format!(
                r#"{{"type":"thread_migrated","payload":{{"id":"{thread}","title":"Fix auth","repo_count":1,"parent_thread_id":null,"kind":"top","review_verdict":null}}}}"#
            )
        );
        roundtrip_eq(&repo);
    }

    #[test]
    fn approval_channel_json_shape() {
        assert_eq!(
            serde_json::to_string(&ClientMessage::ApprovalHook {
                token: "tok".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
            })
            .unwrap(),
            r#"{"type":"approval_hook","payload":{"token":"tok","tool_name":"Bash","tool_input":{"command":"ls"}}}"#
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::ApprovalHookDecision {
                decision: ApprovalDecision::AllowSession,
                message: None,
                interrupt: false,
            })
            .unwrap(),
            r#"{"type":"approval_hook_decision","payload":{"decision":"allow_session","message":null,"interrupt":false}}"#
        );
        let envelope = DaemonMessage::AgentEventEnvelope {
            thread_id: thread_id(),
            event: AgentEvent::ApprovalRequested {
                request_id: "apr-1".into(),
                tool_name: "Bash".into(),
                input_json: serde_json::json!({"command": "ls"}),
            },
        };
        roundtrip_json(&envelope);
    }

    #[test]
    fn app_dir_prefers_trama_data_dir_over_home() {
        // Single test touching these variables, so no cross-test races.
        let saved = (std::env::var_os(DATA_DIR_ENV), std::env::var_os("HOME"));
        unsafe {
            std::env::set_var("HOME", "/Users/someone");
            std::env::remove_var(DATA_DIR_ENV);
        }
        assert_eq!(
            default_app_dir(),
            Some(PathBuf::from(
                "/Users/someone/Library/Application Support/TramaWorkspace"
            ))
        );
        unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/trama-test") };
        assert_eq!(default_app_dir(), Some(PathBuf::from("/tmp/trama-test")));
        unsafe {
            match saved.0 {
                Some(value) => std::env::set_var(DATA_DIR_ENV, value),
                None => std::env::remove_var(DATA_DIR_ENV),
            }
            match saved.1 {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn protocol_version_is_five() {
        assert_eq!(PROTOCOL_VERSION, 5);
    }

    #[test]
    fn subthread_contract_shapes() {
        let parent = thread_id();
        let sub = ThreadId::from("01hsub000000000000000000001");
        let client = [
            ClientMessage::CreateSubthread {
                parent_thread_id: parent.clone(),
                kind: SubthreadKind::Review,
                title: Some("Review".into()),
                initial_prompt: None,
            },
            ClientMessage::ListSubthreads {
                parent_thread_id: parent.clone(),
            },
            ClientMessage::GetChanges {
                thread_id: sub.clone(),
                base: ChangeBase::LastTurn,
            },
            ClientMessage::PullChanges {
                subthread_id: sub.clone(),
            },
            ClientMessage::SubmitReviewVerdict {
                review_thread_id: sub.clone(),
                verdict: ReviewVerdict::RequestChanges {
                    summary: "add tests".into(),
                },
            },
        ];
        for msg in &client {
            roundtrip_eq(msg);
            roundtrip_json(msg);
        }
        assert_eq!(
            serde_json::to_value(&client[0]).unwrap(),
            serde_json::json!({"type":"create_subthread","payload":{
                "parent_thread_id": parent.to_string(), "kind":"review",
                "title":"Review", "initial_prompt": null}})
        );
        // Optional fields and the default base may be omitted.
        let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
            "type":"get_changes","payload":{"thread_id": sub.to_string()}}))
        .unwrap();
        assert_eq!(
            minimal,
            ClientMessage::GetChanges {
                thread_id: sub.clone(),
                base: ChangeBase::ThreadBase
            }
        );
        assert_eq!(
            serde_json::to_value(&client[4]).unwrap()["payload"]["verdict"],
            serde_json::json!({"verdict":"request_changes","summary":"add tests"})
        );

        let child = ThreadSnapshot {
            id: sub.clone(),
            title: "Review".into(),
            repo_count: 1,
            parent_thread_id: Some(parent.clone()),
            kind: ThreadKind::Review,
            review_verdict: Some(ReviewVerdict::Approve),
        };
        let daemon = [
            DaemonMessage::SubthreadCreated(child.clone()),
            DaemonMessage::Subthreads {
                parent_thread_id: parent.clone(),
                threads: vec![child.clone()],
            },
            DaemonMessage::Changes {
                thread_id: sub.clone(),
                base: ChangeBase::ThreadBase,
                repos: vec![RepoChanges {
                    repository_id: RepositoryId::from("repo"),
                    title: "app".into(),
                    base_revision: "abc".into(),
                    diff: "diff --git a/x b/x\n".into(),
                    truncated: false,
                    files: vec![FileChange {
                        path: "x".into(),
                        old_path: None,
                        status: FileChangeStatus::Modified,
                        additions: Some(1),
                        deletions: Some(0),
                    }],
                }],
            },
            DaemonMessage::ChangesPulled {
                subthread_id: sub.clone(),
                parent_thread_id: parent.clone(),
                repos: vec![RepoPullResult {
                    repository_id: RepositoryId::from("repo"),
                    files: vec![PullFileResult {
                        path: "x".into(),
                        change: FileChangeStatus::Modified,
                        outcome: PullFileOutcome::Conflict {
                            markers_written: true,
                            reason: "both".into(),
                        },
                    }],
                }],
            },
            DaemonMessage::ReviewVerdictPosted {
                review_thread_id: sub.clone(),
                parent_thread_id: parent.clone(),
                verdict: ReviewVerdict::Approve,
            },
            DaemonMessage::LandResult {
                land_thread_id: sub,
                parent_thread_id: parent,
                outcome: LandOutcome::Landed,
                summary: "merged".into(),
            },
        ];
        for msg in &daemon {
            roundtrip_json(msg);
        }
        let json = serde_json::to_value(&daemon[0]).unwrap();
        assert_eq!(json["type"], "subthread_created");
        assert_eq!(json["payload"]["kind"], "review");
        assert_eq!(json["payload"]["review_verdict"]["verdict"], "approve");
        // Snapshots from older peers without the new fields decode as top-level.
        let legacy: ThreadSnapshot = serde_json::from_value(serde_json::json!({
            "id": "t", "title": "old", "repo_count": 1}))
        .unwrap();
        assert_eq!(legacy.kind, ThreadKind::Top);
        assert!(legacy.parent_thread_id.is_none());
    }

    #[test]
    fn thread_checkout_discovery_contract_uses_snake_case_status() {
        let message = DaemonMessage::ThreadCheckouts {
            thread_id: ThreadId::from("thread-1"),
            checkouts: vec![CheckoutSnapshot {
                repository_id: RepositoryId::from("repo-1"),
                title: "backend".into(),
                source_path: PathBuf::from("/src/backend"),
                checkout_path: Some(PathBuf::from("/src/backend/.trama/worktrees/t/c")),
                status: CheckoutStatus::Ready,
                detail: None,
            }],
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["type"], "thread_checkouts");
        assert_eq!(json["payload"]["checkouts"][0]["status"], "ready");
        let decoded = serde_json::from_value::<DaemonMessage>(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), json);
    }
}
